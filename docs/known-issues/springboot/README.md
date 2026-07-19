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

> Closure update (2026-07-18): the HTTP codec, JDBC, Micrometer tracing, and
> R2DBC `FilteredClassLoader` cluster is fixed. Canonical records are under
> [`docs/internal/springboot`](../../internal/springboot/); the legacy index
> links below are retained as redirects.

> Closure update (2026-07-19): the `Archive`/`Launcher` classpath URL
> enumeration cluster is fixed (`JarFileArchive`/`ExplodedArchive`/
> `ExecutableArchiveLauncher`). One pre-existing residual remains, tracked
> by `propertieslauncher-loader-path-ignored-wrong-app-launched.md`.

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


## 2026-07-17 rerun: 510-class set vs first-ever same-scope HotSpot baseline (429 CratonVM-specific)

Reran the 510 classes still not `PASS` as of the 2026-07-16 snapshot against
current `dev` (`c45d868ac`), plus — for the first time — a same-scope real
HotSpot baseline, to separate genuine CratonVM gaps from pre-existing Spring
Boot test issues. Full methodology, runner bugfixes (a PowerShell `$Args`
name collision that silently broke `-Setup` for an unknown number of prior
sessions, plus a missing `--add-opens` flag), and totals in
`apps/spring-boot-suite-runner/RESULTS-20260717.md`.

**429 of the 510 confirmed CratonVM-specific** (FAIL 304/HANG 120/CRASH 5;
the other 81 were either already-fixed-since-07-16 or fail identically on
HotSpot too, i.e. not CratonVM's fault). Investigated in parallel (18
agents, one per module or module-group) and documented below — every doc in
this batch is dated 2026-07-17 unless noted otherwise. One runner-adjacent
bug (`%n` hardcoding `\n` instead of the platform line separator) was fixed
and verified live during this round; see
[`../../internal/springboot/printf-percent-n-hardcoded-lf-not-platform-separator-FIXED.md`](../../internal/springboot/printf-percent-n-hardcoded-lf-not-platform-separator-FIXED.md).
The `HV000203`/`ArgumentValueValueExtractor` pair (`spring-boot-actuator`'s
`ControllerEndpointDiscovererTests` and `spring-boot-graphql-test`'s
`GraphQlTest{,Properties}IntegrationTests`) was also fixed and verified
(11/11 tests pass) on 2026-07-18 — see
[`../../internal/springboot/controllerendpointdiscoverertests-hv000203-valueextractor-FIXED.md`](../../internal/springboot/controllerendpointdiscoverertests-hv000203-valueextractor-FIXED.md)
and
[`../../internal/springboot/graphql-hibernate-validator-valueextractor-annotatedtype-gap-FIXED.md`](../../internal/springboot/graphql-hibernate-validator-valueextractor-annotatedtype-gap-FIXED.md);
root cause was CratonVM's `Class.getAnnotatedInterfaces()` never modeling
TYPE_USE annotations nested more than one generic level deep.

Several clusters recur across many modules and are the dominant themes this
round — read these first if triaging or planning fix work, since they
explain a large fraction of the 429:

- The JUnit5 `InterceptingExecutableInvoker` livelock cluster is **FIXED 2026-07-18**: [`../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md) records the receiver-type guard added to the downcall adapter fast path and the JIT/`--nojit` evidence.
- [`thread-dump-endpoint-jmx-threadinfo-fidelity.md`](thread-dump-endpoint-jmx-threadinfo-fidelity.md) — OPEN: the liveness portion of `ThreadDumpEndpointTests` is fixed, but its separate JMX lock/monitor diagnostic data remains incomplete.
- The `Class.getMethods()` override-shadowing cluster is **FIXED 2026-07-17**: [`../../internal/springboot/class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md`](../../internal/springboot/class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md) records the implementation and validation. The related disposable-bean, JDBC embedded-data-source, and task-scheduling records moved alongside it. [`jooq-destroy-method-ambiguity-and-hang.md`](jooq-destroy-method-ambiguity-and-hang.md) remains listed only for its separate unresolved hang (its destroy-method Cluster A is fixed).
- [`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md) — classes using `@ClassPathExclusions`/`@ClassPathOverrides` hang instead of crashing, likely because a same-day sibling fix (`wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`) let a previously-crashing Aether/HTTPS artifact-resolution path actually attempt real (now-blocked) network I/O. Explicitly **not** the whole explanation for the livelock cluster above — the two were confused early in triage and later separated with source evidence.
- `CapturedOutput`/`OutputCaptureExtension` sees empty or stale output across ~20 classes in a dozen+ modules — **root-caused and FIXED 2026-07-18**: `ch/qos/logback/classic/Logger`/`LoggerContext.getLogger` and `org/apache/commons/logging/LogFactory`/`Log` were natively stubbed to throwaway objects that never reached `System.out`/`System.err`. See `docs/internal/springboot/conditionevaluationreport-capturedoutput-empty-cluster-FIXED.md` and `docs/internal/springboot/docker-compose-lifecycle-capturedoutput-log-gap-FIXED.md`. [`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md) stays OPEN — most of its classes now pass, but a `core/spring-boot`-concentrated residual remains (multiple distinct new shapes, not yet root-caused). Two new narrower residuals filed: [`oncondition-report-window-isolation-residual.md`](oncondition-report-window-isolation-residual.md), [`propertiesmigration-logfactory-oom-residual.md`](propertiesmigration-logfactory-oom-residual.md).
- PKCS12/PEM keystore parsing failing against demonstrably-correct passwords/keys is **FIXED 2026-07-19**, all sub-clusters: `../../internal/springboot/ssl-pem-pkcs12-store-parse-failure-cluster-FIXED.md` (curve-aware XDH/EdDSA `KeyFactory`, PBES2 `SecretKeyFactory`, real SHA-256 PKCS12 MAC) and `../../internal/springboot/webserversslbundletests-pkcs12-mac-verification-failure-FIXED.md` (same MAC fix, `WebServerSslBundleTests`/`SslMeterBinderTests`/`SslInfoTests`/`JksSslStoreBundleTests`). `SslConnectorCustomizerTests`, cross-filed in the first doc as suspected corroborating evidence, turned out unrelated — see [`rustls-cbc-cipher-suites-not-supported.md`](rustls-cbc-cipher-suites-not-supported.md).

Full per-doc index (98 docs from this round; status strings truncated —
open each doc for the full picture):

| Doc | Status |
|---|---|
| [`batch-jdbc-mergedannotation-isdirectlypresent-abstractmethoderror.md`](batch-jdbc-mergedannotation-isdirectlypresent-abstractmethoderror.md) | OPEN — found 2026-07-17, root cause not pinned to a file:line |
| [`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md) | OPEN (majority FIXED 2026-07-18) — found 2026-07-17 |
| [`oncondition-report-window-isolation-residual.md`](oncondition-report-window-isolation-residual.md) | OPEN — found 2026-07-18, residual of the capturedoutput fix above |
| [`propertiesmigration-logfactory-oom-residual.md`](propertiesmigration-logfactory-oom-residual.md) | OPEN — found 2026-07-18, residual of the capturedoutput fix above |
| [Cassandra JNI `ThrowNew` payload-loss hang (fixed)](../../internal/fixed-suite-bugs/cassandra-jni-thrownew-discards-payload-hang-FIXED.md) | FIXED — 2026-07-17 |
| [`CertificateMatcherTests` DSA KeyPairGenerator gap](../../internal/fixed-suite-bugs/springboot-certificatematchertests-dsa-keypairgenerator-FIXED.md) | FIXED — 2026-07-17 |
| [`Class.getMethods` override-shadowing duplicate-close cluster](../../internal/springboot/class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md) | FIXED — 2026-07-17 |
| [`Collections.singletonMap` real-wrapper regression](../../internal/springboot/collections-singletonmap-hashmap-backed-not-real-class-FIXED.md) | FIXED — 2026-07-18 |
| [`cacheautoconfigurationtests-infinispan-null-cachemanager-residual.md`](cacheautoconfigurationtests-infinispan-null-cachemanager-residual.md) | OPEN — found 2026-07-17, residual of the contextrunner-resource-cycle re-triage below |
| `contextrunner-resource-cycle-then-silent-stall-cluster.md` | **REFUTED/FIXED 2026-07-17** — moved to [`../../internal/springboot/contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`](../../internal/springboot/contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md); not a deadlock — live CPU sampling showed 5 of 6 classes just needed more wall time than the 300s shard default (now carved out in the suite runner), and the `Class.getMethods()` override-shadowing fix cut both failures and wall time substantially. The 6th class is a distinct recursion bug, see [`opentelemetry-contextstorage-early-init-recursion-cluster.md`](opentelemetry-contextstorage-early-init-recursion-cluster.md). Real residuals split into 3 new docs (this row, plus the two below) |
| [`controllerendpointdiscoverertests-hv000203-valueextractor.md`](controllerendpointdiscoverertests-hv000203-valueextractor.md) | OPEN — found 2026-07-17, hypothesis unconfirmed |
| [`core-autoconfigure-singleton-fail-residuals-20260717.md`](core-autoconfigure-singleton-fail-residuals-20260717.md) | OPEN — found 2026-07-17 |
| [`ConfigData resource-resolution empty cluster`](../../internal/springboot/core-spring-boot-configdata-resource-resolution-empty-cluster.md) | FIXED — 2026-07-18 |
| [`core-spring-boot-crossthread-throwable-stacktrace-loss.md`](core-spring-boot-crossthread-throwable-stacktrace-loss.md) | OPEN — found 2026-07-17 (root cause confirmed at file:line precision |
| `JsonWriterTests` unmodifiable-map lambda `ClassCastException` | **FIXED 2026-07-18** — moved to [`../../internal/springboot/core-spring-boot-jsonwriter-unmodifiablemap-classcast-FIXED.md`](../../internal/springboot/core-spring-boot-jsonwriter-unmodifiablemap-classcast-FIXED.md); VM-generated lambda-bridge cast errors now use the same concrete collection class name as `Object.getClass()`, allowing Spring's `LambdaSafe` generic filter to suppress expected map-vs-`String` mismatches |
| [`core-spring-boot-test-config-data-and-classpath-scan-cluster.md`](core-spring-boot-test-config-data-and-classpath-scan-cluster.md) | OPEN — found 2026-07-17, none root-caused to a CratonVM file:line ye |
| [`crashfail-20260717-crash-cluster.md`](crashfail-20260717-crash-cluster.md) | OPEN — found 2026-07-17 |
| [`data-jdbc-id-field-misclassified-as-association.md`](data-jdbc-id-field-misclassified-as-association.md) | OPEN — found 2026-07-17. Hypothesis 1 below (a `Class`-identity/equa |
| [`datajdbctestintegrationtests-association-from-reference-type-npe.md`](datajdbctestintegrationtests-association-from-reference-type-npe.md) | OPEN — found 2026-07-17 |
| [`disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster.md`](disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster.md) | OPEN — found 2026-07-17 |
| [`docker-compose-lifecycle-capturedoutput-log-gap.md`](../../internal/springboot/docker-compose-lifecycle-capturedoutput-log-gap-FIXED.md) | FIXED — 2026-07-18 |
| [`docker-compose-regex-string-join-charsequence-truncation-cluster.md`](docker-compose-regex-string-join-charsequence-truncation-cluster.md) | OPEN — found 2026-07-17 |
| [`docker-compose-socketinputstream-read-timedout-as-eof.md`](docker-compose-socketinputstream-read-timedout-as-eof.md) | OPEN — found 2026-07-17 |
| `EncodePasswordCommandTests` BCrypt verifier stall | **FIXED 2026-07-18** — moved to [`../../internal/springboot/encodepasswordcommandtests-cli-hang-FIXED.md`](../../internal/springboot/encodepasswordcommandtests-cli-hang-FIXED.md); Spring Security's BCrypt key schedule now uses the native intrinsic while legacy `$2x$` compatibility retains bytecode semantics |
| [`file-url-openconnection-getinputstream-unknownserviceexception-FIXED.md`](../../internal/springboot/file-url-openconnection-getinputstream-unknownserviceexception-FIXED.md) | FIXED — 2026-07-17 |
| `flyway-resourceprovidercustomizer-aot-substitution-not-applied.md` | **FIXED 2026-07-18** — moved to [`../../internal/springboot/flyway-resourceprovidercustomizer-aot-substitution-not-applied-FIXED.md`](../../internal/springboot/flyway-resourceprovidercustomizer-aot-substitution-not-applied-FIXED.md); reflective descriptor resolution now honors the defining `ClassLoader` before any global fallback |
| [`graphql-security-autoconfiguration-early-hang-FIXED.md`](../../internal/springboot/graphql-security-autoconfiguration-early-hang-FIXED.md) | FIXED — 2026-07-18; generated GraphQL lambda classes now retain defining-host package metadata |
| [`grpc-test-springextension-isbeanoverride-nosuchmethoderror.md`](grpc-test-springextension-isbeanoverride-nosuchmethoderror.md) | OPEN — found 2026-07-17 |
| [`hateoas-stream-reduce-triarg-missing-native-abstractmethoderror-FIXED.md`](../../internal/springboot/hateoas-stream-reduce-triarg-missing-native-abstractmethoderror-FIXED.md) | FIXED — 2026-07-18 |
| [`hazelcast-socketchannel-bind-and-server-hang.md`](hazelcast-socketchannel-bind-and-server-hang.md) | OPEN — found 2026-07-17 |
| [`hibernatejpaautoconfigurationtests-stall-hang-FIXED.md`](../../internal/springboot/hibernatejpaautoconfigurationtests-stall-hang-FIXED.md) | FIXED — 2026-07-18 |
| [`http-codec-filteredclassloader-condition-not-honored.md`](http-codec-filteredclassloader-condition-not-honored.md) | OPEN — found 2026-07-17. Same mechanism as 3 already-filed sibling d |
| [`http-converter-stream-reduce-3arg-no-code-attribute-FIXED.md`](../../internal/springboot/http-converter-stream-reduce-3arg-no-code-attribute-FIXED.md) | FIXED — 2026-07-18 |
| [`HTTP client autoconfigure classpath-presence cluster`](../../internal/springboot/httpclient-autoconfigure-classpath-presence-cluster-FIXED.md) | FIXED — 2026-07-18 |
| [`modifiedclasspath-override-artifact-identity-regression-FIXED.md`](../../internal/springboot/modifiedclasspath-override-artifact-identity-regression-FIXED.md) | FIXED — 2026-07-18; protection domains now retain loader-local CodeSource identity and isolated resource exclusions |
| [`inetaddressfilter-null-socketaddress-overload-not-throwing.md`](inetaddressfilter-null-socketaddress-overload-not-throwing.md) | OPEN — found 2026-07-17 (hypothesis, not confirmed to file:line) |
| [`instant-force-native-factory-synthetic-tostring-cluster.md`](instant-force-native-factory-synthetic-tostring-cluster.md) | OPEN — found 2026-07-17 (confirmed at source level) |
| [`integration-mbeanserver-getdomains-missing-native-abstractmethoderror.md`](integration-mbeanserver-getdomains-missing-native-abstractmethoderror.md) | OPEN — found 2026-07-17 |
| [`Jackson/Json Mixin Module Entries AOT TestCompiler mismatch`](../../internal/springboot/jacksonmixinmoduleentries-aot-testcompiler-mismatch-FIXED.md) | FIXED — 2026-07-18; the Spring AOT descriptor loader-identity repair also resolves both sibling Jackson AOT classes |
| [`jarmode-tools-extractlayers-timestamp-preservation.md`](jarmode-tools-extractlayers-timestamp-preservation.md) | OPEN — found 2026-07-17. Hypothesis only, not confirmed. |
| [`jarmode-tools manifest copy and launcher attributes`](../../internal/springboot/jarmode-tools-manifest-start-class-lost-FIXED.md) | FIXED — 2026-07-18; copy isolation, folded manifest attributes, and the package-directory resource residual are covered |
| [`jdbc-hikari-mbean-not-registered-cluster.md`](jdbc-hikari-mbean-not-registered-cluster.md) | OPEN — found 2026-07-17 |
| [`jdbc-hikariconfig-copystateto-field-access-cluster.md`](jdbc-hikariconfig-copystateto-field-access-cluster.md) | OPEN — found 2026-07-17 |
| [`jdbc-mail-jndi-custom-initialcontextfactory-not-consulted-cluster.md`](jdbc-mail-jndi-custom-initialcontextfactory-not-consulted-cluster.md) | OPEN — found 2026-07-17 (confirms/root-causes an unconfirmed hypothe |
| [`jdbc-oracle-ucp-pool-init-hang.md`](jdbc-oracle-ucp-pool-init-hang.md) | OPEN — found 2026-07-17 |
| [`jdk-httpclient-builder-config-loss-cluster-FIXED.md`](../../internal/springboot/jdk-httpclient-builder-config-loss-cluster-FIXED.md) | FIXED — 2026-07-18 |
| [`jdkclienthttpsender-response-timeout-not-enforced.md`](jdkclienthttpsender-response-timeout-not-enforced.md) | OPEN — found 2026-07-17 (hypothesis, not traced into CratonVM's HTTP |
| [`jetty-loaderhidingresourcetests-empty-jar-listing.md`](jetty-loaderhidingresourcetests-empty-jar-listing.md) | OPEN — found 2026-07-17 |
| [`jetty-private-lambda-wrong-receiver-startcontext-recursion-cluster-FIXED.md`](../../internal/springboot/jetty-private-lambda-wrong-receiver-startcontext-recursion-cluster-FIXED.md) | FIXED — 2026-07-18 |
| [`jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals.md`](jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals.md) | MOSTLY FIXED — reflective-supertype residual + 4 real bugs (deflate SYNC_FLUSH, wildcard connect target, deflate-after-finish corruption, dead-thread-owned-monitor hang) fixed 2026-07-18; `JettyReactiveWebServerFactoryTests` now completes clean; `JettyServletWebServerFactoryTests` hits a newly-exposed, unrelated OPEN bug (blocking socket read ignoring SO_TIMEOUT) |
| [`jooq-destroy-method-ambiguity-and-hang.md`](jooq-destroy-method-ambiguity-and-hang.md) | PARTIALLY FIXED — Cluster A fixed 2026-07-17; unrelated hang remains OPEN |
| [`jsonreadertests-deprecation-reason-string-truncation.md`](jsonreadertests-deprecation-reason-string-truncation.md) | OPEN — found 2026-07-17 |
| [`../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md) | FIXED — 2026-07-18 |
| [`jvmmetrics-virtualthreadmetrics-jfr-recordingstream-unimplemented.md`](jvmmetrics-virtualthreadmetrics-jfr-recordingstream-unimplemented.md) | OPEN — found 2026-07-17 (root cause well-grounded via project roadma |
| [`kafkametrics-reentrantreadwritelock-newcondition-nosuchmethoderror.md`](kafkametrics-reentrantreadwritelock-newcondition-nosuchmethoderror.md) | OPEN — found 2026-07-17 (hypothesis, not confirmed to file:line) |
| [`ldap-sslsocketfactory-createsocket-inetaddress-abstractmethoderror.md`](ldap-sslsocketfactory-createsocket-inetaddress-abstractmethoderror.md) | OPEN — found 2026-07-17 |
| [`loader-tools-manifest-entries-and-zip-fidelity-residuals.md`](loader-tools-manifest-entries-and-zip-fidelity-residuals.md) | OPEN — found 2026-07-17 |
| [`loader-tools Spring-Boot-Version manifest attribute`](../../internal/springboot/loader-tools-spring-boot-version-manifest-attribute-missing-FIXED.md) | FIXED — 2026-07-18; `Attributes` now retains legal null-valued manifest entries |
| [`messagesourceautoconfigurationtests-getmessage-default-fallback.md`](messagesourceautoconfigurationtests-getmessage-default-fallback.md) | OPEN — found 2026-07-17 |
| [`micrometer-tracing-filteredclassloader-condition-not-honored.md`](micrometer-tracing-filteredclassloader-condition-not-honored.md) | OPEN — found 2026-07-17, not root-caused |
| `MockMvcSecurityIntegrationTests` known-user Basic-auth 401 | **FIXED 2026-07-18** — moved to [`../../internal/springboot/mockmvcsecurity-basicauth-knownuser-401-FIXED.md`](../../internal/springboot/mockmvcsecurity-basicauth-knownuser-401-FIXED.md); the Spring Security BCrypt intrinsic now proves the real credential-verification path in both JIT and `--nojit` modes |
| `MockWebEnvironmentServletComponentScanIntegrationTests` | **FIXED 2026-07-18** — original livelock record and its loader/annotation/reflection follow-ons moved to [`../../internal/springboot/mockwebenvironmentservletcomponentscanintegrationtests-hang-FIXED.md`](../../internal/springboot/mockwebenvironmentservletcomponentscanintegrationtests-hang-FIXED.md) |
| [`thread-dump-endpoint-jmx-threadinfo-fidelity.md`](thread-dump-endpoint-jmx-threadinfo-fidelity.md) | OPEN — found 2026-07-18; separate JMX diagnostic fidelity gap |
| [`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md) | OPEN — found 2026-07-17 |
| [`mongodb-dns-resolver-null-nameservers-npe-and-reactive-hang.md`](mongodb-dns-resolver-null-nameservers-npe-and-reactive-hang.md) | OPEN — found 2026-07-17 |
| [`nettyrsocketserverfactorytests-bindexception-os-error-10049.md`](nettyrsocketserverfactorytests-bindexception-os-error-10049.md) | OPEN — found 2026-07-17 |
| [`objectname-getkeypropertylist-ca-kp-array-npe-residual.md`](objectname-getkeypropertylist-ca-kp-array-npe-residual.md) | OPEN — found 2026-07-17 (residual of a FIXED sibling bug) |
| [`opentelemetry-contextstorage-early-init-recursion-cluster.md`](opentelemetry-contextstorage-early-init-recursion-cluster.md) | OPEN — found 2026-07-17, residual of the contextrunner-resource-cycle re-triage below |
| [`otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang.md`](otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang.md) | OPEN — found 2026-07-17 (hypothesis, unconfirmed — no thread dump  |
| [`propertieslauncher-loader-path-ignored-wrong-app-launched.md`](propertieslauncher-loader-path-ignored-wrong-app-launched.md) | OPEN — found 2026-07-17, hypothesis only (root cause not pinned to s |
| [`pulsar-propertiesmapper-timeunit-null-npe.md`](pulsar-propertiesmapper-timeunit-null-npe.md) | OPEN — found 2026-07-17 |
| [`quartzautoconfigurationtests-jdbc-jobstore-not-applied.md`](quartzautoconfigurationtests-jdbc-jobstore-not-applied.md) | OPEN — found 2026-07-17 |
| [`r2dbc-filteredclassloader-loadclass-override-bypassed.md`](r2dbc-filteredclassloader-loadclass-override-bypassed.md) | OPEN — found 2026-07-17 |
| [`rabbitautoconfigurationtests-cglib-enhance-hang.md`](rabbitautoconfigurationtests-cglib-enhance-hang.md) | OPEN — found 2026-07-17 |
| [`reactor-netty-server-startup-hang.md`](reactor-netty-server-startup-hang.md) | OPEN — found 2026-07-17. Hypothesis only, not root-caused. |
| [`repeatablecontainers-method-cache-classcastexception.md`](repeatablecontainers-method-cache-classcastexception.md) | OPEN — found 2026-07-17, hypothesis unconfirmed |
| [`resourcestests-trailing-slash-windows-path-error.md`](resourcestests-trailing-slash-windows-path-error.md) | OPEN — found 2026-07-17 (hypothesis, not confirmed against native `j |
| [`security-saml2-package-version-npe-and-x509key-unknown-algo.md`](security-saml2-package-version-npe-and-x509key-unknown-algo.md) | OPEN — found 2026-07-17 |
| `ServletComponentScanIntegrationTests` registration | **FIXED 2026-07-18, regressed and re-FIXED 2026-07-19** — bare `URLClassLoader.getResource()` skipped parent delegation, breaking `indexedComponentsAreRegistered()`; current authoritative closure at [`../../internal/springboot/urlclassloader-getresource-bare-instance-parent-delegation-skip-FIXED.md`](../../internal/springboot/urlclassloader-getresource-bare-instance-parent-delegation-skip-FIXED.md) (earlier records: [`servletcomponentscanintegrationtests-registration-verified-FIXED.md`](../../internal/springboot/servletcomponentscanintegrationtests-registration-verified-FIXED.md), [`urlclassloader-getresourceasstream-real-jdk-mode-dead-FIXED.md`](../../internal/springboot/urlclassloader-getresourceasstream-real-jdk-mode-dead-FIXED.md)) |
| [`spring-boot-cloudfoundry-rerun-20260717.md`](spring-boot-cloudfoundry-rerun-20260717.md) | OPEN — found 2026-07-17 |
| [`spring-boot-configuration-processor-testcompiler-hang-cluster.md`](spring-boot-configuration-processor-testcompiler-hang-cluster.md) | OPEN — found 2026-07-17 |
| [`spring-boot-devtools-residual-fails-cluster.md`](spring-boot-devtools-residual-fails-cluster.md) | OPEN — found 2026-07-17 |
| [`spring-boot-health-rerun-20260717.md`](spring-boot-health-rerun-20260717.md) | OPEN — found 2026-07-17 |
| `Archive`/`Launcher` classpath URL enumeration empty/wrong | **FIXED 2026-07-19** — verified 10/10 `JarFileArchiveTests`, 4/4 `WarLauncherTests`, 7/7 `ExplodedArchiveTests`, 5/5 `JarLauncherTests`; moved to [`../../internal/springboot/spring-boot-loader-classpath-url-enumeration-empty-cluster-FIXED.md`](../../internal/springboot/spring-boot-loader-classpath-url-enumeration-empty-cluster-FIXED.md). Residual: `propertieslauncher-loader-path-ignored-wrong-app-launched.md` (8 of its original 11 fixed as a side effect, 4 remain, still OPEN) |
| [`spring-boot-loader-zipfile-close-invokespecial-native-bypass-npe.md`](spring-boot-loader-zipfile-close-invokespecial-native-bypass-npe.md) | OPEN — found 2026-07-17 |
| [`spring-boot-restclient-residuals.md`](spring-boot-restclient-residuals.md) | OPEN — found 2026-07-17 |
| Spring proxy layout-probe livelock blocking broad HttpClient consumers | **FIXED 2026-07-18** — JUnit adapter receiver admission now prevents the unrelated field probe; verified in JIT and `--nojit`; moved to [`../../internal/springboot/spring-proxy-layout-livelock-blocking-httpclient-consumers-FIXED.md`](../../internal/springboot/spring-proxy-layout-livelock-blocking-httpclient-consumers-FIXED.md) |
| `module/spring-boot-tomcat` 2026-07-17 rerun: 3 unrelated FAILs + throughput-wall HANGs | **FIXED/CLOSED 2026-07-19** — WAR-rooted `URLClassLoader` resource resolution fixed (narrow URL-format residual noted inline), Tomcat metrics MBean binding fixed upstream, `SslConnectorCustomizerTests` root-caused to the rustls-CBC-cipher-suite gap below (not a bug in this doc's scope); throughput wall unchanged (not a bug); moved to [`../../internal/springboot/spring-boot-tomcat-rerun-20260717-residuals-FIXED.md`](../../internal/springboot/spring-boot-tomcat-rerun-20260717-residuals-FIXED.md) |
| `RecordableServerHttpRequestTests.getRemoteAddress()` NPE + `WebFluxManagementChildContextConfigurationIntegrationTests` HANG | **FIXED 2026-07-19** — `InetSocketAddress(int)` now synthesizes a resolved wildcard address; `getfield`/`putfield` loader-aware field resolution + `MergedAnnotation$Adapt.isIn` cross-loader identity bridge closed the hang; verified 5/5 and 3x clean 4/5 completions respectively; moved to [`../../internal/springboot/spring-boot-webflux-residuals-FIXED.md`](../../internal/springboot/spring-boot-webflux-residuals-FIXED.md). New, unrelated residual discovered in current `dev` (same class, different/later hang) — see [`webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md`](webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md) |
| [`webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md`](webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md) | OPEN — found 2026-07-19, pre-existing `dev` regression unrelated to the fix above, not yet bisected |
| PEM private-key parse + PKCS12 keystore load (3 JCA/keystore gaps) | **FIXED 2026-07-19** — curve-aware XDH/EdDSA `KeyFactory`, PBES2 `SecretKeyFactory`, real SHA-256 PKCS12 MAC; moved to [`../../internal/springboot/ssl-pem-pkcs12-store-parse-failure-cluster-FIXED.md`](../../internal/springboot/ssl-pem-pkcs12-store-parse-failure-cluster-FIXED.md). `SslConnectorCustomizerTests`, originally cross-filed here, turned out unrelated — see `rustls-cbc-cipher-suites-not-supported.md` below |
| [`rustls-cbc-cipher-suites-not-supported.md`](rustls-cbc-cipher-suites-not-supported.md) | OPEN — found 2026-07-19; rustls (CratonVM's TLS backend) never implements CBC-mode cipher suites, a permanent environmental limitation, not a bug |
| `SslMeterBinderTests` gauge count doubles after `SslBundle` update | **FIXED 2026-07-19** — PKCS#12 `Pkcs8ShroudedKeyBag` decrypt failure dropped the whole `PrivateKeyEntry` instead of keeping an encrypted placeholder, and cert chains were never extended past the leaf by issuer/subject DN matching; moved to [`../../internal/springboot/sslmeterbindertests-gauge-duplication-on-bundle-update-FIXED.md`](../../internal/springboot/sslmeterbindertests-gauge-duplication-on-bundle-update-FIXED.md) |
| `StaticResourceJarsTests` JAR/URL-encoded-path resource lookup failures | **FIXED 2026-07-18** — `File(URI)` percent-decoding + `JarURLConnection`/`JarFile` caching and closed-state tracking; verified in JIT and `--nojit`; moved to [`../../internal/springboot/staticresourcejarstests-jar-url-handling-cluster-FIXED.md`](../../internal/springboot/staticresourcejarstests-jar-url-handling-cluster-FIXED.md) |
| Thymeleaf Groovy layout-dialect `DecorateProcessor` constructor mismatch + missing `CapturedOutput` warning | **FIXED 2026-07-19** — `invokedynamic`'s generic `MethodHandle.invoke` bridge erased trailing `boolean` call-site args to `Integer`; preserved the real target descriptor and boxed `Z` args as `Boolean`; `CapturedOutput` half was already fixed by the project-wide Logger/LogFactory cluster; verified in JIT; moved to [`../../internal/springboot/thymeleaf-groovy-layoutdialect-cluster-FIXED.md`](../../internal/springboot/thymeleaf-groovy-layoutdialect-cluster-FIXED.md) |
| [`thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md`](thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md) | OPEN — found 2026-07-19, newly exposed by the fix above; one real root cause found+fixed 2026-07-19 (GC reference-queue field collision), hang persists one level deeper (see doc) |
| `"literal:" + aPath` dead-dispatched to `Object.toString()` via `String.valueOf`/indy string concat | **FIXED 2026-07-19** — `String.valueOf(Object)`'s real `obj.toString()` call site never retargeted onto the receiver's actual class (CP-symbolic class `java/lang/Object` isn't interface/abstract); added a receiver-aware `toString()` check ahead of the resolved `class_name`; verified `ThymeleafReactiveAutoConfigurationTests` 21/21 (up from 20/21); moved to [`../../internal/springboot/path-tostring-indy-stringconcat-dead-dispatch-FIXED.md`](../../internal/springboot/path-tostring-indy-stringconcat-dead-dispatch-FIXED.md) |
| [`tls-sslbundle-trust-validation-gap-cluster.md`](tls-sslbundle-trust-validation-gap-cluster.md) | OPEN — found 2026-07-17 |
| [`web-server-mockito-restub-no-op-cluster.md`](web-server-mockito-restub-no-op-cluster.md) | OPEN — found 2026-07-17 |
| [`webclient-loopback-self-connect-timeout-os10060-cluster.md`](webclient-loopback-self-connect-timeout-os10060-cluster.md) | OPEN — found 2026-07-17, residual of the contextrunner-resource-cycle re-triage below |
| [`webmvc-error-forward-and-multiboot-timeout-cluster.md`](webmvc-error-forward-and-multiboot-timeout-cluster.md) | OPEN — found 2026-07-17 |
| [`zipkin-realsocket-retry-spin-hang-FIXED.md`](../../internal/springboot/zipkin-realsocket-retry-spin-hang-FIXED.md) | FIXED 2026-07-18 — Zipkin binary request-body preservation and exact gzip/DEFLATE fidelity. |
| [`webmvc-test-anonymous-tostring-override-not-dispatched.md`](webmvc-test-anonymous-tostring-override-not-dispatched.md) | OPEN — found 2026-07-17 (hypothesis for the dispatch gap; the format |
| [`WebServerSslBundleTests`/`SslMeterBinderTests` PKCS#12 MAC verification failure](../../internal/springboot/webserversslbundletests-pkcs12-mac-verification-failure-FIXED.md) | FIXED — 2026-07-19 |
| [`zipkin-realsocket-retry-spin-hang.md`](zipkin-realsocket-retry-spin-hang.md) | OPEN — found 2026-07-17. Hypothesis only, not root-caused. |
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
