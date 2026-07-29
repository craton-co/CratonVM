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
> R2DBC `FilteredClassLoader` cluster is fixed. Canonical write-ups for those
> have since been archived; the legacy index links below are retained as
> redirects.

> Closure update (2026-07-19): the `Archive`/`Launcher` classpath URL
> enumeration cluster is fixed (`JarFileArchive`/`ExplodedArchive`/
> `ExecutableArchiveLauncher`). One pre-existing residual remains, tracked
> by `propertieslauncher-loader-path-ignored-wrong-app-launched.md`.

> Closure update (2026-07-20): `repeatablecontainers-method-cache-classcastexception.md`
> no longer reproduces on current dev — closed, moved to
> `repeatablecontainers-method-cache-classcastexception-FIXED.md`.
> Its affected class now fails LATER for a different, unrelated reason (new
> doc: `mockresolver-dynamicclassloader-classnotfound-forked-testcontext.md`).
> Also filed a new CRITICAL, non-Spring-specific core VM/JIT finding from the
> same investigation: back-edge OSR compilation could silently re-execute
> loop iterations after an `invokedynamic` trap. **FIXED 2026-07-20** — see
> `jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md`.

> Closure update (2026-07-20): `webclient-loopback-self-connect-timeout-os10060-cluster.md`
> is fixed — closed, moved to
> `webclient-loopback-self-connect-timeout-os10060-cluster-FIXED.md`.
> Root cause pinned to `SocketChannel.connect()`'s non-blocking
> deferred-failure path falsely reporting synchronous success; already fixed
> by `49d7834e9` (reactor-netty startup-hang residuals fix), confirmed via
> reproduction rather than code reading alone.

> Closure update (2026-07-23): the `module/spring-boot-data-redis` 4-class
> HANG cluster (`DataRedisAutoConfigurationTests`,
> `DataRedisAutoConfigurationJedisTests`,
> `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests`,
> `DataRedisHealthContributorAutoConfigurationTests` — see
> `RESULTS-20260723.md`'s residual table) is fixed — see
> `data-redis-urlclassloader-uncached-classpath-hang-FIXED.md`.
> Root cause: `URLClassLoader.findClass`/`findResource` rebuilt the whole
> classpath scan from scratch on every call (no caching), and `JarFile`
> entry lookups eagerly decompressed every entry in a jar just to answer an
> existence check — both general classloading bugs, not Redis-specific,
> just tipped over the 300s timeout by this module's unusually large
> (~121-jar) test classpath. Fixed in `native-builtins/src/classloader.rs`
> and `phases_late.rs`. One pre-existing, narrower residual unmasked by the
> fix (not caused by it) — **also now FIXED 2026-07-23**, moved to
> `data-redis-jedis-sslbundle-withpackageresources-classloader-leak-FIXED.md`.
> Root cause: `classloader_real.rs::cl_real_load_class_base` silently
> swallowed a user-defined parent loader's authoritative
> `ClassNotFoundException` and fell through to CratonVM's loader-blind flat
> global class store, un-doing `ModifiedClassPathClassLoader`'s
> `@ClassPathExclusions` filtering.

> Closure update (2026-07-27): `mockito-silently-selects-fallback-location-and-memberaccessor.md`
> is fixed — moved to
> `mockito-silently-selects-fallback-location-and-memberaccessor-FIXED.md`.
> Root cause was **not** the branch/dispatch defect that doc hypothesised, and
> not classloader-related at all (it reproduces with no fork loader at all):
> CratonVM shipped two deliberate native overrides that replaced Mockito's
> selector methods wholesale — `LocationFactory.create` returned a
> `Java8LocationImpl` carrying the hardcoded string `"-> at <<unknown line>>"`
> instead of walking the stack, and `ModuleMemberAccessor.delegate` always
> returned `ReflectionMemberAccessor`. Both are now off by default (behind
> `CRATONVM_MOCKITO_LEGACY_SELECTORS`), so every Mockito failure message names
> its real call site again. Removing the first override unmasked a second,
> wider bug also fixed here: CratonVM's synthetic `StackWalker$StackFrame`
> carrier never registered `toString()`, so it printed
> `java.lang.StackWalker$StackFrame@a166` instead of `Cls.method(File:line)`
> for **any** consumer, not just Mockito.

| Doc | Classes | Severity | Status |
|---|---:|---|---|
| `OnClassCondition.addAll` NPE-cast-to-`String[]` | 75 (348 occurrences) | CRITICAL | **FIXED/RETIRED 2026-07-13** — moved to `onclasscondition-npe-cast-string-array-cluster-FIXED.md`; `@ConditionalOnClass`'s unresolvable-`Class`-element handling now defers to a `TypeNotPresentException` sentinel matching HotSpot, instead of a bare `null`. Verified against all 75/75 originally-affected classes |
| `DisposableBeanAdapter` "Invalid destruction signature" | 34 | HIGH | **RESOLVED 2026-07-12** — moved to `spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`; a direct probe against current dev found the destroy-method reflection path works fine, closing the cluster (no distinct residual identified) |
| `sun.misc.Unsafe$MemoryAccessOption` NPE | 26 | HIGH | **FIXED/RETIRED 2026-07-12** — moved to `spring-boot-unsafe-memoryaccessoption-npe-FIXED.md`; same bug independently found+fixed via a concurrent Keycloak investigation, canonical doc is `testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md` in the same directory |
| `HttpClient.Builder` dead registration | 13 | HIGH | **RESOLVED 2026-07-12** — moved to `springboot-httpclient-builder-dead-registration-abstractmethoderror-FIXED.md`; the active real-JDK registrar now covers every Java 17 fluent builder method |
| `zip-filedatablock-bulk-bytebuffer-put-aioobe.md` | 9 (whole `spring-boot-loader` module) | HIGH | **FIXED 2026-07-12** — moved to `zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md` |
| `flyway-cglib-heap-corruption-sigsegv-crash.md` | 1 (`FlywayAutoConfigurationTests`) | HIGH (SIGSEGV) | **FIXED 2026-07-12** — moved to `flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md`; `read_string` misidentified a `String[]` array as `java/lang/String` by class ID alone |
| `TestCompiler` annotation/platform listing cluster | 7 (`spring-boot-configuration-processor`) | MEDIUM | **FIXED/RETIRED 2026-07-13** — moved to `testcompiler-annotation-classes-not-found-cluster-FIXED.md`; full JRT package listing and generated in-memory `resource:` URL handling fixed, 94/94 tests pass |
| `BraveAutoConfigurationTests` summary-printing `ClassCastException` | 1 | LOW | **FIXED/RETIRED 2026-07-15** — moved to `brave-baggagefields-classcast-summary-printing-FIXED.md`; fixed the early real-`Collections` initialization path and the receiver-polymorphic `Object.equals` dispatch residual. Verified 26/26 with JIT both off and on |
| `JdbcSessionAutoConfigurationTests` HSQLDB `RangeGroupEmpty` loop | 1 | MEDIUM | **FIXED 2026-07-15** — moved to `jdbcsession-hsqldb-rangegroupempty-hang-FIXED.md`; `java.lang.reflect.Array.set` now validates primitive-array values before unboxing, so HSQLDB's non-wrapper `RangeGroupEmpty` never reaches a speculative field-0 read |
| `CharBuffer.order()` missing native + IDN `<clinit>` poisoning | 6 FAIL + 1 fatal CRASH | HIGH | **FIXED 2026-07-14** — moved to `charbuffer-order-missing-native-idn-clinit-cluster-FIXED.md`; registered `order()` on `CharBuffer` and its 4 `ByteBufferAsCharBuffer{B,L,RB,RL}` views. Verified via standalone repro — the `AbstractMethodError` is gone. **Residual found while verifying**: a deeper, previously-masked bug now surfaces — see [`charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe.md`](charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe.md) below; `java.net.IDN` still does not work end-to-end |
| `CharBuffer.getArray` `ScopedMemoryAccess.copyMemory` AIOOBE | blocks all 7 classes above | HIGH | **FIXED 2026-07-15** — moved to `charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe-FIXED.md`; `s2_bb_as_char_buffer` never seeded the view's `Buffer.address` (`= 16`), so the real bulk-get fast path decoded an offset below the array base. `java.net.IDN` now works end-to-end (== HotSpot), unblocking the whole Netty/RSocket reactive cluster this poisoned |
| `LoggerContext.loggerContextListenerList` null cluster | 20+ | HIGH | **FIXED/RETIRED 2026-07-15** — moved to `logback-loggercontext-listenerlist-final-field-corruption-FIXED.md`; the SLF4J bridge returned a real-classed but unconstructed `LoggerContext` because it overrode `<init>` as a no-op. The bridge now invokes the real constructor; `BannerTests` passes 6/6 under both `--nojit` and JIT |
| `Map.Entry::getKey`/`getValue` lambda-dispatch precedence | 5 | HIGH | **FIXED 2026-07-14** — moved to `structured-logging-map-entry-getkey-lambda-dispatch-precedence-FIXED.md`; the receiver-specific "C25 rescue" native lookup (`vm/src/vm/vm_exec.rs`) no longer skips itself when a less-specific interface-level native already matched. Verified via standalone repro (correctly returns `"spring"` instead of the whole entry) and a regression check of the `Enumeration$Impl.hasMoreElements()` case this rescue was originally written for (still passes) |
| `*ApplicationContextRunnerTests` lazy CGLIB class-not-found process crash | 3 (fatal CRASH) | HIGH | **FIXED 2026-07-15** — moved to `applicationcontextrunnertests-lazy-cglib-classnotfound-crash-FIXED.md`; `Class.forName` now initializes the class resolved through the requested loader by its `ClassId`, preserving visibility of newly defined `$$SpringCGLIB$$` proxies. The registration bridge also propagates duplicate-definition errors. All three original runner classes pass in JIT and `--nojit` |
| [`jit-dispatch-depth-guard-shallow-stackoverflow-cluster.md`](jit-dispatch-depth-guard-shallow-stackoverflow-cluster.md) | 3 | MEDIUM | OPEN — found 2026-07-14. All 3 `StackOverflowError`s have anomalously shallow (3-11 frame), non-repeating traces on trivial, non-recursive Spring APIs — not a credible genuine-recursion shape. Traced to `JIT_DISPATCH_DEPTH`'s guard (`vm/src/jit/helpers.rs`) never recording frame metadata, so any `StackOverflowError` raised through it is structurally incapable of showing its real cause; a separate, unused `CallStack` module with correct frame tracking exists but is dead code |
| [`comparable-classcast-lambda-proxy-unknown-class.md`](comparable-classcast-lambda-proxy-unknown-class.md) | 2 | MEDIUM | OPEN — found 2026-07-14. `Collections.sort`/`Arrays.sort`'s `Comparable` check (`native-collections`) only consults `class_manager`, which never registers lambda-proxy classes — so a lambda implementing a `Comparable`-extending functional interface is falsely rejected and reported as class `<unknown>`. Same root-cause family as an already-fixed sibling bug in `Class.getGenericInterfaces()` |
| [`collectionbindertests-classcast-testdescriptor-crash.md`](collectionbindertests-classcast-testdescriptor-crash.md) | 1 (fatal CRASH) | CRITICAL | OPEN — found 2026-07-14. `CollectionBinderTests` crashes the whole process with `ClassCastException: String cannot be cast to TestDescriptor` entirely inside JUnit Platform's own internal engine-failure-reporting path (0 tests ever ran) — a "wrong-type-from-native-collection" shape in the same family as the already-fixed `OnClassCondition` corruption, but a distinct, unconfirmed occurrence; leading suspect is the new `HASHMAP_NATIVE_DISPATCH_CACHE` fast path from the same-day `77f8b37e5` |
| `JsonValueWriterTests` cyclic collection nesting guard | 1 (fatal CRASH) | CRITICAL | **FIXED 2026-07-15** — moved to `jsonvaluewritertests-nesting-depth-guard-stack-overflow-FIXED.md`; native `Map.forEach` now snapshots cyclic map entries without hashing them, and `Iterable` method-reference lambdas use their real iterator implementation |
| `HttpClientSecure` null-provider crash | 2 (fatal CRASH) | HIGH | **FIXED 2026-07-16** — moved to `reactor-nettyhttpclient-httpclientsecure-null-provider-crash-FIXED.md`; native client SSL session support, JKS key recovery, TLS alert delivery, real Tomcat lifecycle, and MethodHandle primitive-return handling now cover the former crash and residuals.
| [`method-getexceptiontypes-null-jdk-dynamic-proxy-synthesized-method.md`](method-getexceptiontypes-null-jdk-dynamic-proxy-synthesized-method.md) | 3 (4 test methods) | MEDIUM | OPEN — found 2026-07-16 (`rerun-20260716`, shard3). `Method.getExceptionTypes()` returns `null` (JDK spec: never null) for the synthetic `Method` object `vm/src/vm/vm_exec.rs::proxy_invoke_handler` builds to pass into `InvocationHandler.invoke()` for JDK dynamic proxies — it never sets the `exceptionTypes` field, unlike the sibling `create_method_object` path which already fixed this exact gap. Hits every Spring Data repository call (`JdkDynamicAopProxy`) via `PersistenceExceptionTranslationInterceptor.invoke` → `ReflectionUtils.declaresException` |
| `TestEngine.getId()` young-GC forwarding-walk truncation | 2 (fatal CRASH) | CRITICAL | **FIXED 2026-07-16** — moved to `testengine-getid-abstractmethoderror-young-gc-forwarding-gap-FIXED.md`. Found and root-caused same-day as a new young-GC pre-forwarding walk (`gc/src/gen_heap.rs`) that only special-cased the `GAP_FILLER_CLASS_ID` sentinel, unlike the sibling `exact_cursor` walk which also consults the free-list via `skip_free_blocks` — dropping live young-gen objects from the forwarding set on unrecognized free/TLAB-remnant ranges. `fix/wildfly-cce0079-close-20260716` (adds the missing `skip_free_blocks` call) merged into `dev` the same day; verified fixed via rebuild + rerun (fatal crash gone) |
| `BasicErrorControllerIntegrationTests` stale-pointer crash | 1 (fatal CRASH) | CRITICAL | **FIXED 2026-07-16** — moved to `basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md`. Independent confirmation of the same young-GC forwarding-walk truncation bug as the `TestEngine.getId()` entry above. Verified fixed via rebuild + rerun after `fix/wildfly-cce0079-close-20260716` merged into `dev` (fatal `ClassCastException`/CRASH gone) |
| [`wrong-receiver-virtual-dispatch-corruption-cluster.md`](wrong-receiver-virtual-dispatch-corruption-cluster.md) | 9 FAIL + 1 fatal CRASH | CRITICAL (Case 1) / HIGH unconfirmed (Case 2) | OPEN — found 2026-07-16. Case 1 (`String.setOption`, 9 classes, root-caused): `javax/net/ssl/SSLSocketFactory.createSocket()` (`native-builtins/src/tls.rs`) still hands out a bare 2-field synthetic `Socket` under `CRATONVM_REAL_NET_SOCKETS=1`; real `Socket.getImpl()` bytecode then reads garbage off the undersized object and dispatches onto a leftover `String` — a gap in a previously-fixed sibling bug (`javax/net/SocketFactory`, commit `bd03eb243`) that never covered the SSL variant. Case 2 (`File.get()`, 1 fatal CRASH) has the same symptom shape but is **not confirmed** to share Case 1's mechanism — filed for tracking, root cause still open |
| [`tomcatservletwebserverfactory-cross-module-classnotfound-crash.md`](tomcatservletwebserverfactory-cross-module-classnotfound-crash.md) | 10 (fatal CRASH) | HIGH | OPEN — found 2026-07-16. A native shim (`native-builtins/src/net_phase_e.rs`, `ServletWebServerApplicationContext.getWebServerFactory()`) unconditionally allocates a hardcoded `TomcatServletWebServerFactory` regardless of servlet backend — added to route around a real Tomcat bean-registration bug, but fires identically for Jetty-only/generic-web-server modules where that class genuinely doesn't exist on the classpath (confirmed via real Gradle classpath dumps, not a suite-runner gap). The resulting class-not-found escapes as an uncaught internal error and aborts the process instead of throwing a catchable `NoClassDefFoundError` |




## 2026-07-28/29 rerun: the 139-class residual, 5x timeout (116 now PASS, 2 new CRASH)

Reran the 139-class residual from the round below (99 FAIL + 40 HANG)
against current `dev`, after merging/rebuilding, at 5x the normal timeout
(1500s vs. 300s) to separate genuinely-stuck HANGs from just-slow ones.
**116/139 (83.5%) now PASS.** Residual down to 20 FAIL + 2 new CRASH + 1
confirmed-still-stuck HANG. The 2 CRASHes are worth immediate attention:
`BasicErrorControllerIntegrationTests` (same class as the silently-
reverted `http_parse_url` fix, now crashing instead of failing) and
`OriginTrackedYamlLoaderTests` (a genuine `OutOfMemoryError`, previously
dismissed in the 07-11 round as a suspected runner artifact — that
dismissal is now questionable given a concrete OOM with a specific
allocation size). Not re-triaged against the docs below this round — a
rerun-only pass. Full before/after table and reproduce instructions in
`apps/spring-boot-suite-runner/RESULTS-20260728.md`.

## 2026-07-23 rerun: the 429 CratonVM-specific classes, 6 days later (290 now PASS)

Reran exactly the 429 classes confirmed CratonVM-specific in the round
below, after merging `dev` forward (moved substantially in 6 days) and
rebuilding. **290/429 (67.6%) now PASS**, residual down to 99 FAIL + 40
HANG, **0 CRASH** (all 5 previously-fatal crashes resolved — 4 now PASS,
1 now FAIL but no longer fatal). Full before/after table, residual module
breakdown, and reproduce instructions in
`apps/spring-boot-suite-runner/RESULTS-20260723.md`.

**Follow-up triage (same day):** investigated the 85 of 99 residual FAIL
classes outside `loader/spring-boot-loader` (out of scope this round; the
40 HANG classes also not covered here), in parallel across 5 investigation
passes (core/spring-boot dedicated, 4 balanced groups for the rest). Found
that **several docs marked FIXED from the 07-17 round have genuinely
regressed** — not stale/false triage, but real re-breaks, including one
traced to a specific silent-merge culprit commit (`http_parse_url`'s
query-only-path fix got carried back to a pre-fix state by an unrelated
later merge). Notes added to the affected FIXED docs under
`docs/internal/fixed-suite-bugs/` rather than reopening them wholesale.
22 new docs filed for genuinely uncovered clusters, several confirmed at
file:line (`SSLContext.getDefault()` caching gap, hardcoded `"Thread"`
worker names bypassing `DefaultThreadFactory`, `Console.ttyStatus()`
missing native registration, `Files.readAttributes` ignoring the requested
attribute-view type on Windows, UTF-16 BOM re-emitted per-call, and more)
— see the full doc list below, all dated 2026-07-23.

- `core-spring-boot-base64-decoder-message-mismatch-20260723.md` — Base64 decode error message text doesn't match real JDK ("Invalid base64 char" vs "Illegal base64"). **FIXED 2026-07-28** — moved to [`../../internal/fixed-suite-bugs/springboot/core-spring-boot-base64-decoder-message-mismatch-20260723-FIXED.md`](../../internal/fixed-suite-bugs/springboot/core-spring-boot-base64-decoder-message-mismatch-20260723-FIXED.md). The filed message text was already corrected by the core39 clusterB/clusterD fixes (both classes green on unmodified `dev`); the residual JDK-parity gaps in the same decoder — MIME rejecting instead of ignoring illegal bytes, four distinct malformed-input messages, the missing trailing-byte check, the signed illegal-char hex, `decode(String)`'s ISO-8859-1 mapping, and the MIME encoder's phantom trailing CRLF — are fixed here. `java.util.Base64` now matches HotSpot 25 byte-for-byte across a 94-case probe
- ~~`core-spring-boot-beandefinitionloader-package-scan-empty-20260723.md`~~ — **CLOSED 2026-07-28**, retired to [`../../internal/fixed-suite-bugs/springboot/core-spring-boot-beandefinitionloader-package-scan-empty-FIXED.md`](../../internal/fixed-suite-bugs/springboot/core-spring-boot-beandefinitionloader-package-scan-empty-FIXED.md). The filed root cause (a pathing-jar directory-listing gap) was **wrong** — the pathing jar was never involved. The real one: `ClassLoader.getDefinedPackage(String)` returned unconditional `null`, and that is both `BeanDefinitionLoader.findPackage()`'s final answer and `isLoadCandidate()`'s only package-directory-vs-XML guard. Fixed on dev by `2a44df5cc`, which postdates the run that filed this doc; three follow-on fidelity residuals in the replacement native (`Package` interning, `getDefinedPackages()` self-consistency, loader-scoped visibility) fixed 2026-07-28.
- [`core-spring-boot-console-ttystatus-missing-native-20260723.md`](core-spring-boot-console-ttystatus-missing-native-20260723.md) — `java/io/Console.ttyStatus()I` has no native registration — `Console.<clinit>` fails under Mockito instrumentation
- [`core-spring-boot-crossmethod-state-leakage-residuals-20260723.md`](core-spring-boot-crossmethod-state-leakage-residuals-20260723.md) — Possible cross-test-method state leakage within a single SbRunner process (pattern observation)
- `core-spring-boot-keystore-provider-name-swallowed-20260723.md` — `KeyStore.getInstance(type, unregisteredProviderName)` loses the provider name from its exception. **FIXED 2026-07-28** — moved to [`../../internal/fixed-suite-bugs/springboot/core-spring-boot-keystore-provider-name-swallowed-20260723-FIXED.md`](../../internal/fixed-suite-bugs/springboot/core-spring-boot-keystore-provider-name-swallowed-20260723-FIXED.md). The two filed methods were fixed by core39 cluster D; the doc's own "could not find a native override of `GetInstance`" note was the miss — it lives in `native-builtins/src/jca/provider_chain.rs`, which a `src/*.rs` glob skips. The **same defect survived on four other engines** (`KeyFactory`/`Signature`/`SecureRandom`/`Cipher` discarded the provider argument entirely) and is fixed here, along with `MessageDigest.getInstance(algo, "SUN")` failing outright and `MessageDigest`'s unsupported-algorithm throw being an unchecked `SecurityException` instead of `NoSuchAlgorithmException`
- [`jca-provider-algorithm-ownership-not-enforced-20260728.md`](jca-provider-algorithm-ownership-not-enforced-20260728.md) — `getInstance(algorithm, registeredProvider)` succeeds even when that provider does not offer the algorithm (residual split out of the doc above; no test currently red)
- [`core-spring-boot-modifiedclasspath-uniqueid-discovery-failure-20260723.md`](core-spring-boot-modifiedclasspath-uniqueid-discovery-failure-20260723.md) — `ModifiedClassPathExtension`'s nested UniqueId discovery fails for every test method
- [`core-spring-boot-properties-computeifabsent-null-map-npe-20260723.md`](core-spring-boot-properties-computeifabsent-null-map-npe-20260723.md) — `java.util.Properties.computeIfAbsent` NPEs on `this.map` for a binder-instantiated `Properties` target
- [`core-spring-boot-properties-parser-drops-formfeed-20260723.md`](core-spring-boot-properties-parser-drops-formfeed-20260723.md) — OriginTrackedPropertiesLoader silently drops a literal form-feed byte from a property value
- [`core-spring-boot-uncategorized-residuals-20260723.md`](core-spring-boot-uncategorized-residuals-20260723.md) — `core/spring-boot` 2026-07-23 rerun — uncategorized individual residuals
- [`core-spring-boot-utf16-encoder-bom-restated-every-call-20260723.md`](core-spring-boot-utf16-encoder-bom-restated-every-call-20260723.md) — UTF-16 `CharsetEncoder.encode()` re-emits the byte-order-mark on every call instead of once per stream
- ~~`core-spring-boot-windows-posix-fileattributes-classcast-20260723.md`~~ — **CLOSED 2026-07-28**, retired to [`../../internal/fixed-suite-bugs/springboot/core-spring-boot-windows-posix-fileattributes-classcast-20260723-FIXED.md`](../../internal/fixed-suite-bugs/springboot/core-spring-boot-windows-posix-fileattributes-classcast-20260723-FIXED.md). The native generic attribute bridge now validates the requested type on Windows and rejects POSIX attributes with `UnsupportedOperationException`.
- [`http-parse-url-query-only-authority-split-regression-20260723.md`](http-parse-url-query-only-authority-split-regression-20260723.md) — `http_parse_url` lost its query-only-path fix in a silent merge — `BasicErrorControllerIntegrationTests` "bad port" failures are back
- [`oracleucp-poolsizeoneconnection-connection-pool-empty-20260723.md`](oracleucp-poolsizeoneconnection-connection-pool-empty-20260723.md) — `OracleUcpDataSourcePoolMetadataTests.getPoolSizeOneConnection` — first on-demand UCP connection borrow reports "pool is empty"
- ~~`otlpexemplarsautoconfigurationtests-empty-histogram-output-20260723.md`~~ — **CLOSED 2026-07-28**, retired to [`../../internal/fixed-suite-bugs/springboot/otlpexemplarsautoconfigurationtests-empty-histogram-output-FIXED-20260728.md`](../../internal/fixed-suite-bugs/springboot/otlpexemplarsautoconfigurationtests-empty-histogram-output-FIXED-20260728.md). `Collections.singletonList(null)` was incorrectly snapshotted as empty by the native bulk-collection bridge, leaving null exemplar entries in `ArrayList.removeAll`; the real singleton element is now preserved and the class passes 6/6 in both modes.
- ~~`r2dbcautoconfigurationtests-properties-map-not-applied-20260723.md`~~ — **CLOSED 2026-07-28**, retired to [`../../internal/fixed-suite-bugs/springboot/r2dbcautoconfigurationtests-properties-map-not-applied-20260723-FIXED-20260728.md`](../../internal/fixed-suite-bugs/springboot/r2dbcautoconfigurationtests-properties-map-not-applied-20260723-FIXED-20260728.md). Receiver-rooted interface default dispatch now preserves the iterable configuration-property source required by map binding.
- ~~`springbootcontextloaderaottests-groovy-metaclass-nullfields-20260723.md`~~ — **CLOSED 2026-07-28**, retired to [`../../internal/fixed-suite-bugs/springboot/springbootcontextloaderaottests-groovy-metaclass-nullfields-FIXED-20260728.md`](../../internal/fixed-suite-bugs/springboot/springbootcontextloaderaottests-groovy-metaclass-nullfields-FIXED-20260728.md). A fresh current-dev executable passes the AOT class and all six compiled Groovy bootstrap regressions in both JIT modes (43/43 each).
- [`sslcontext-getdefault-implicit-caching-gap-20260723.md`](sslcontext-getdefault-implicit-caching-gap-20260723.md) — `SSLContext.getDefault()` allocates a fresh context every call unless `setDefault()` was explicitly invoked first — breaks JDK singleton-default contract
- [`taskscheduling-scheduledexecutorservice-literal-thread-name-20260723.md`](taskscheduling-scheduledexecutorservice-literal-thread-name-20260723.md) — `ScheduledExecutorService` worker threads get the literal name `"Thread"` instead of `pool-N-thread-M`
- [`webmvcendpointmanagementcontextconfigurationtests-multipartresolver-selfmatch-residual-20260723.md`](webmvcendpointmanagementcontextconfigurationtests-multipartresolver-selfmatch-residual-20260723.md) — `WebMvcEndpointManagementContextConfigurationTests`: `multipartResolver` factory-method parameter self-resolves to the bean being created

> Closure update (2026-07-23): `module/spring-boot-security`'s 4-class
> residual from this rerun (`SecurityFilterAutoConfigurationEarlyInitializationTests`,
> `PathRequestTests`, `ManagementWebSecurityAutoConfigurationTests`,
> `ReactiveManagementWebSecurityAutoConfigurationTests`) — root cause was a
> `ModifiedClassPathClassLoader` built from a "pathing JAR" launch (used when
> a module's classpath is too long for a Windows command line) resolving to
> an effectively empty classpath, since `ClassPath::new` never expanded a
> plain jar's own manifest `Class-Path:` attribute. **FIXED** — see
> `springboot-security-modifiedclasspathextension-pathing-jar-classpath-manifest-FIXED.md`.
> `PathRequestTests` now passes outright; the other 3 narrowed to two distinct
> residuals: `onbeancondition-mergedannotations-intermittent-identity-mismatch-FIXED.md`
> (now FIXED, see the 2026-07-27 closure update below) and `securityfilterautoconfig-capturedoutput-password-not-observed.md`
> (was here, now **FIXED** — see below).

> Closure update (2026-07-26): `SecurityFilterAutoConfigurationEarlyInitializationTests`
> (the `securityfilterautoconfig-capturedoutput-password-not-observed.md` residual
> above) is fixed — verified 5/5 PASS against current `dev`, resolved as a side
> effect of unrelated classloader/reflection drift between 2026-07-23 and
> 2026-07-26, not independently root-caused. Moved to
> `securityfilterautoconfig-capturedoutput-password-not-observed-FIXED.md`.
> The sibling `onbeancondition-mergedannotations-intermittent-identity-mismatch.md`
> residual remains OPEN (independently spot-checked the same session:
> `ManagementWebSecurityAutoConfigurationTests` 3/3 clean; the reactive variant
> shows a separate, likely-unrelated intermittent timeout — see the FIXED doc
> above for details).

> Closure update (2026-07-27): `onbeancondition-mergedannotations-intermittent-identity-mismatch.md`
> is now closed too. The doc's own claimed symptom never reproduced (23 clean
> runs total, JIT on/off) — resolved as a side effect of unrelated `dev`
> drift, same pattern as the entries above. The reactive variant's "separate,
> likely-unrelated intermittent timeout" flagged in the closure update just
> above turned out to be real and root-caused: a `reactor/core/scheduler/
> NonBlocking` classloader double-define race (a background Reactor
> scheduler thread's recursive interface resolution racing, unsynchronized,
> against the main thread's own top-level `Class.forName` on the same
> isolated loader) — fixed in both `classloading::ClassManager::
> resolve_supertype` and `native-builtins`'s `ucl_try_define_local_class`.
> Verified 0/40 clean (was 10-23% failure rate) plus 16 more clean runs
> across both classes with JIT on/off. Moved to
> `onbeancondition-mergedannotations-intermittent-identity-mismatch-FIXED.md`.

> Investigation (2026-07-24): `module/spring-boot-micrometer-tracing-opentelemetry`'s
> 2-class residual from this rerun (`OpenTelemetryBaggagePropagationIntegrationTests`,
> `OpenTelemetryTracingAutoConfigurationTests`) — the former's 5 failing
> parameterized cases all throw a CratonVM NPE from inside AssertJ's own
> error-formatting path (`WritableAssertionInfo.representation` ends up null
> only when `StringAssert`/`AbstractCharSequenceAssert` objects are
> constructed via `Assertions.assertThat(String)`, not via direct
> construction — reduced to a 100% reproducing ~15-line standalone repro, not
> yet root-caused to file:line, likely masking real failure messages broadly
> across the suite since `assertThat(someString)` is ubiquitous). **CLOSED
> 2026-07-26** — moved to
> `micrometer-tracing-opentelemetry-assertj-representation-npe-and-eventpublisher-residuals-FIXED.md`.
> The NPE was CratonVM's own `native_assertj_lightweight_comparable_assert`
> shim building `AbstractAssert` without running its constructor (fixed by
> `fdc852f558`, bisect-confirmed); the two real failures it had been masking
> were a single loader-blind lambda-impl resolution in the native-callback
> dispatcher (`vm/src/vm/vm_exec.rs`). The module is now 9/9 classes PASS.

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
`printf-percent-n-hardcoded-lf-not-platform-separator-FIXED.md`.
The `HV000203`/`ArgumentValueValueExtractor` pair (`spring-boot-actuator`'s
`ControllerEndpointDiscovererTests` and `spring-boot-graphql-test`'s
`GraphQlTest{,Properties}IntegrationTests`) was also fixed and verified
(11/11 tests pass) on 2026-07-18 — see
`controllerendpointdiscoverertests-hv000203-valueextractor-FIXED.md`
and
`graphql-hibernate-validator-valueextractor-annotatedtype-gap-FIXED.md`;
root cause was CratonVM's `Class.getAnnotatedInterfaces()` never modeling
TYPE_USE annotations nested more than one generic level deep.

Several clusters recur across many modules and are the dominant themes this
round — read these first if triaging or planning fix work, since they
explain a large fraction of the 429:

- The JUnit5 `InterceptingExecutableInvoker` livelock cluster is **FIXED 2026-07-18**: `junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md` records the receiver-type guard added to the downcall adapter fast path and the JIT/`--nojit` evidence.
- [`thread-dump-endpoint-jmx-threadinfo-fidelity.md`](thread-dump-endpoint-jmx-threadinfo-fidelity.md) — OPEN: the liveness portion of `ThreadDumpEndpointTests` is fixed, but its separate JMX lock/monitor diagnostic data remains incomplete.
- The `Class.getMethods()` override-shadowing cluster is **FIXED 2026-07-17**: `class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md` records the implementation and validation. The related disposable-bean, JDBC embedded-data-source, and task-scheduling records moved alongside it. [`jooq-destroy-method-ambiguity-and-hang.md`](jooq-destroy-method-ambiguity-and-hang.md) remains listed only for its separate unresolved hang (its destroy-method Cluster A is fixed).
- [`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md) — classes using `@ClassPathExclusions`/`@ClassPathOverrides` hang instead of crashing, likely because a same-day sibling fix (`wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`) let a previously-crashing Aether/HTTPS artifact-resolution path actually attempt real (now-blocked) network I/O. Explicitly **not** the whole explanation for the livelock cluster above — the two were confused early in triage and later separated with source evidence.
- `CapturedOutput`/`OutputCaptureExtension` sees empty or stale output across ~20 classes in a dozen+ modules — **root-caused and FIXED 2026-07-18**: `ch/qos/logback/classic/Logger`/`LoggerContext.getLogger` and `org/apache/commons/logging/LogFactory`/`Log` were natively stubbed to throwaway objects that never reached `System.out`/`System.err`. See `conditionevaluationreport-capturedoutput-empty-cluster-FIXED.md` and `docker-compose-lifecycle-capturedoutput-log-gap-FIXED.md`. [`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md) stays OPEN — most of its classes now pass, but a `core/spring-boot`-concentrated residual remains (multiple distinct new shapes, not yet root-caused). Two new narrower residuals filed: [`oncondition-report-window-isolation-residual.md`](oncondition-report-window-isolation-residual.md), [`propertiesmigration-logfactory-oom-residual.md`](propertiesmigration-logfactory-oom-residual.md).
- PKCS12/PEM keystore parsing failing against demonstrably-correct passwords/keys is **FIXED 2026-07-19**, all sub-clusters: `ssl-pem-pkcs12-store-parse-failure-cluster-FIXED.md` (curve-aware XDH/EdDSA `KeyFactory`, PBES2 `SecretKeyFactory`, real SHA-256 PKCS12 MAC) and `webserversslbundletests-pkcs12-mac-verification-failure-FIXED.md` (same MAC fix, `WebServerSslBundleTests`/`SslMeterBinderTests`/`SslInfoTests`/`JksSslStoreBundleTests`). `SslConnectorCustomizerTests`, cross-filed in the first doc as suspected corroborating evidence, turned out unrelated — see `rustls-cbc-cipher-suites-not-supported.md`.

Full per-doc index (98 docs from this round; status strings truncated —
open each doc for the full picture):

| Doc | Status |
|---|---|
| [`batch-jdbc-mergedannotation-isdirectlypresent-abstractmethoderror.md`](batch-jdbc-mergedannotation-isdirectlypresent-abstractmethoderror.md) | OPEN — found 2026-07-17, root cause not pinned to a file:line |
| [`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md) | OPEN (majority FIXED 2026-07-18) — found 2026-07-17 |
| [`oncondition-report-window-isolation-residual.md`](oncondition-report-window-isolation-residual.md) | OPEN — found 2026-07-18, residual of the capturedoutput fix above |
| [`propertiesmigration-logfactory-oom-residual.md`](propertiesmigration-logfactory-oom-residual.md) | OPEN — found 2026-07-18, residual of the capturedoutput fix above |
| Cassandra JNI `ThrowNew` payload-loss hang (fixed) | FIXED — 2026-07-17 |
| `CertificateMatcherTests` DSA KeyPairGenerator gap | FIXED — 2026-07-17 |
| `Class.getMethods` override-shadowing duplicate-close cluster | FIXED — 2026-07-17 |
| `Collections.singletonMap` real-wrapper regression | FIXED — 2026-07-18 |
| Infinispan `getCacheNames()` null-return cluster | FIXED — 2026-07-19 |
| Hazelcast `java.net.http.HttpRequest.method()` AbstractMethodError | FIXED — 2026-07-20 |
| `contextrunner-resource-cycle-then-silent-stall-cluster.md` | **REFUTED/FIXED 2026-07-17** — moved to `contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`; not a deadlock — live CPU sampling showed 5 of 6 classes just needed more wall time than the 300s shard default (now carved out in the suite runner), and the `Class.getMethods()` override-shadowing fix cut both failures and wall time substantially. The 6th class's recursion bug was tracked separately and confirmed already fixed 2026-07-20, see `opentelemetry-contextstorage-early-init-recursion-cluster-FIXED.md`. Real residuals split into 3 new docs (this row, plus the two below) |
| [`controllerendpointdiscoverertests-hv000203-valueextractor.md`](controllerendpointdiscoverertests-hv000203-valueextractor.md) | OPEN — found 2026-07-17, hypothesis unconfirmed |
| [`core-autoconfigure-singleton-fail-residuals-20260717.md`](core-autoconfigure-singleton-fail-residuals-20260717.md) | OPEN — found 2026-07-17 |
| `ConfigData resource-resolution empty cluster` | FIXED — 2026-07-18 |
| [`core-spring-boot-crossthread-throwable-stacktrace-loss.md`](core-spring-boot-crossthread-throwable-stacktrace-loss.md) | OPEN — found 2026-07-17 (root cause confirmed at file:line precision |
| `JsonWriterTests` unmodifiable-map lambda `ClassCastException` | **FIXED 2026-07-18** — moved to `core-spring-boot-jsonwriter-unmodifiablemap-classcast-FIXED.md`; VM-generated lambda-bridge cast errors now use the same concrete collection class name as `Object.getClass()`, allowing Spring's `LambdaSafe` generic filter to suppress expected map-vs-`String` mismatches |
| `core-spring-boot-test config-data / classpath-scan cluster` | **FIXED 2026-07-21** — moved to `core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md`; all clusters + Residuals 1–6 closed (final root cause: JIT `checkcast` name-only resolution silently nulling a cast between duplicate-loader `ClassInfo` copies), `ImportsContextCustomizerFactoryTests` also closed by the same-day genuine56 HashMap/proxy-dispatch fixes |
| [`crashfail-20260717-crash-cluster.md`](crashfail-20260717-crash-cluster.md) | OPEN — found 2026-07-17 |
| [`data-jdbc-id-field-misclassified-as-association.md`](data-jdbc-id-field-misclassified-as-association.md) | OPEN — found 2026-07-17. Hypothesis 1 below (a `Class`-identity/equa |
| [`datajdbctestintegrationtests-association-from-reference-type-npe.md`](datajdbctestintegrationtests-association-from-reference-type-npe.md) | OPEN — found 2026-07-17 |
| [`disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster.md`](disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster.md) | OPEN — found 2026-07-17 |
| `docker-compose-lifecycle-capturedoutput-log-gap.md` | FIXED — 2026-07-18 |
| [`docker-compose-regex-string-join-charsequence-truncation-cluster.md`](docker-compose-regex-string-join-charsequence-truncation-cluster.md) | OPEN — found 2026-07-17 |
| [`docker-compose-socketinputstream-read-timedout-as-eof.md`](docker-compose-socketinputstream-read-timedout-as-eof.md) | OPEN — found 2026-07-17 |
| `embedded-tomcat-loopback-self-connect-silent-hang` | **FIXED 2026-07-20** — moved to `embedded-tomcat-loopback-self-connect-silent-hang-FIXED.md`; commit `3293da963` broadened `invoke_virtual`'s native dispatch to route almost all virtual calls through a lock-heavy path, deadlocking Spring Boot's concurrent `BackgroundPreinitializingApplicationListener` background thread against the main thread during startup; narrowed the dispatch condition back down. `RemappedErrorViewIntegrationTests` 2/2 PASS in 15s (was HANG); the co-located anonymous-`toString()` regression test still passes |
| `EncodePasswordCommandTests` BCrypt verifier stall | **FIXED 2026-07-18** — moved to `encodepasswordcommandtests-cli-hang-FIXED.md`; Spring Security's BCrypt key schedule now uses the native intrinsic while legacy `$2x$` compatibility retains bytecode semantics |
| `file-url-openconnection-getinputstream-unknownserviceexception-FIXED.md` | FIXED — 2026-07-17 |
| `flyway-resourceprovidercustomizer-aot-substitution-not-applied.md` | **FIXED 2026-07-18** — moved to `flyway-resourceprovidercustomizer-aot-substitution-not-applied-FIXED.md`; reflective descriptor resolution now honors the defining `ClassLoader` before any global fallback |
| `graphql-security-autoconfiguration-early-hang-FIXED.md` | FIXED — 2026-07-18; generated GraphQL lambda classes now retain defining-host package metadata |
| [`grpc-test-springextension-isbeanoverride-nosuchmethoderror.md`](grpc-test-springextension-isbeanoverride-nosuchmethoderror.md) | OPEN — found 2026-07-17 |
| `hateoas-stream-reduce-triarg-missing-native-abstractmethoderror-FIXED.md` | FIXED — 2026-07-18 |
| [`hazelcast-socketchannel-bind-and-server-hang.md`](hazelcast-socketchannel-bind-and-server-hang.md) | OPEN — found 2026-07-17 |
| [`h2c-priorknowledge-hpack-headerblock-decode-failure.md`](h2c-priorknowledge-hpack-headerblock-decode-failure.md) | OPEN — found 2026-07-20, residual of the reactor-netty hang fix. Wire bytes verified well-formed by hand; not JIT- or allocator-specific; root cause not identified |
| `hibernatejpaautoconfigurationtests-stall-hang-FIXED.md` | FIXED — 2026-07-18 |
| [`http-codec-filteredclassloader-condition-not-honored.md`](http-codec-filteredclassloader-condition-not-honored.md) | OPEN — found 2026-07-17. Same mechanism as 3 already-filed sibling d |
| `http-converter-stream-reduce-3arg-no-code-attribute-FIXED.md` | FIXED — 2026-07-18 |
| `HTTP client autoconfigure classpath-presence cluster` | FIXED — 2026-07-18 |
| `modifiedclasspath-override-artifact-identity-regression-FIXED.md` | FIXED — 2026-07-18; protection domains now retain loader-local CodeSource identity and isolated resource exclusions |
| [`inetaddressfilter-null-socketaddress-overload-not-throwing.md`](inetaddressfilter-null-socketaddress-overload-not-throwing.md) | OPEN — found 2026-07-17 (hypothesis, not confirmed to file:line) |
| [`instant-force-native-factory-synthetic-tostring-cluster.md`](instant-force-native-factory-synthetic-tostring-cluster.md) | OPEN — found 2026-07-17 (confirmed at source level) |
| [`integration-mbeanserver-getdomains-missing-native-abstractmethoderror.md`](integration-mbeanserver-getdomains-missing-native-abstractmethoderror.md) | OPEN — found 2026-07-17 |
| `Jackson/Json Mixin Module Entries AOT TestCompiler mismatch` | FIXED — 2026-07-18; the Spring AOT descriptor loader-identity repair also resolves both sibling Jackson AOT classes |
| [`jarmode-tools-extractlayers-timestamp-preservation.md`](jarmode-tools-extractlayers-timestamp-preservation.md) | OPEN — found 2026-07-17. Hypothesis only, not confirmed. |
| `jarmode-tools manifest copy and launcher attributes` | FIXED — 2026-07-18; copy isolation, folded manifest attributes, and the package-directory resource residual are covered |
| [`jdbc-hikari-mbean-not-registered-cluster.md`](jdbc-hikari-mbean-not-registered-cluster.md) | OPEN — found 2026-07-17 |
| [`jdbc-hikariconfig-copystateto-field-access-cluster.md`](jdbc-hikariconfig-copystateto-field-access-cluster.md) | OPEN — found 2026-07-17 |
| [`jdbc-mail-jndi-custom-initialcontextfactory-not-consulted-cluster.md`](jdbc-mail-jndi-custom-initialcontextfactory-not-consulted-cluster.md) | OPEN — found 2026-07-17 (confirms/root-causes an unconfirmed hypothe |
| [`jdbc-oracle-ucp-pool-init-hang.md`](jdbc-oracle-ucp-pool-init-hang.md) | OPEN — found 2026-07-17 |
| `jdk-httpclient-builder-config-loss-cluster-FIXED.md` | FIXED — 2026-07-18 |
| [`jdkclienthttpsender-response-timeout-not-enforced.md`](jdkclienthttpsender-response-timeout-not-enforced.md) | OPEN — found 2026-07-17 (hypothesis, not traced into CratonVM's HTTP |
| [`jetty-loaderhidingresourcetests-empty-jar-listing.md`](jetty-loaderhidingresourcetests-empty-jar-listing.md) | OPEN — found 2026-07-17 |
| `jetty-private-lambda-wrong-receiver-startcontext-recursion-cluster-FIXED.md` | FIXED — 2026-07-18 |
| [`jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals.md`](jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals.md) | MOSTLY FIXED — reflective-supertype residual + 4 real bugs (deflate SYNC_FLUSH, wildcard connect target, deflate-after-finish corruption, dead-thread-owned-monitor hang) fixed 2026-07-18; `JettyReactiveWebServerFactoryTests` now completes clean; `JettyServletWebServerFactoryTests` hits a newly-exposed, unrelated OPEN bug (blocking socket read ignoring SO_TIMEOUT) |
| `jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md` | **FIXED 2026-07-20** — core VM/JIT bug (not Spring-specific): an OSR'd loop followed by an `invokedynamic` call (e.g. string-concat `println`) in the same method could silently re-execute the loop's already-committed iterations when the indy trap's OSR-exit transfer was rejected. Fixed by (1) tolerating an unmappable LOCAL slot in the OSR-exit transfer instead of rejecting it whole, and (2) banning OSR for any method containing `invokedynamic` (RBC.7, mirroring the existing `athrow` ban) |
| [`jooq-destroy-method-ambiguity-and-hang.md`](jooq-destroy-method-ambiguity-and-hang.md) | PARTIALLY FIXED — Cluster A fixed 2026-07-17; unrelated hang remains OPEN |
| [`jsonreadertests-deprecation-reason-string-truncation.md`](jsonreadertests-deprecation-reason-string-truncation.md) | OPEN — found 2026-07-17 |
| `junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md` | FIXED — 2026-07-18 |
| [`jvmmetrics-virtualthreadmetrics-jfr-recordingstream-unimplemented.md`](jvmmetrics-virtualthreadmetrics-jfr-recordingstream-unimplemented.md) | OPEN — found 2026-07-17 (root cause well-grounded via project roadma |
| [`kafkametrics-reentrantreadwritelock-newcondition-nosuchmethoderror.md`](kafkametrics-reentrantreadwritelock-newcondition-nosuchmethoderror.md) | OPEN — found 2026-07-17 (hypothesis, not confirmed to file:line) |
| [`ldap-sslsocketfactory-createsocket-inetaddress-abstractmethoderror.md`](ldap-sslsocketfactory-createsocket-inetaddress-abstractmethoderror.md) | OPEN — found 2026-07-17 |
| [`loader-tools-manifest-entries-and-zip-fidelity-residuals.md`](loader-tools-manifest-entries-and-zip-fidelity-residuals.md) | OPEN — found 2026-07-17 |
| `loader-tools Spring-Boot-Version manifest attribute` | FIXED — 2026-07-18; `Attributes` now retains legal null-valued manifest entries |
| [`messagesourceautoconfigurationtests-getmessage-default-fallback.md`](messagesourceautoconfigurationtests-getmessage-default-fallback.md) | OPEN — found 2026-07-17 |
| [`micrometer-tracing-filteredclassloader-condition-not-honored.md`](micrometer-tracing-filteredclassloader-condition-not-honored.md) | OPEN — found 2026-07-17, not root-caused |
| `MockMvcSecurityIntegrationTests` known-user Basic-auth 401 | **FIXED 2026-07-18** — moved to `mockmvcsecurity-basicauth-knownuser-401-FIXED.md`; the Spring Security BCrypt intrinsic now proves the real credential-verification path in both JIT and `--nojit` modes |
| `MockWebEnvironmentServletComponentScanIntegrationTests` | **FIXED 2026-07-18** — original livelock record and its loader/annotation/reflection follow-ons moved to `mockwebenvironmentservletcomponentscanintegrationtests-hang-FIXED.md` |
| [`mockresolver-dynamicclassloader-classnotfound-forked-testcontext.md`](mockresolver-dynamicclassloader-classnotfound-forked-testcontext.md) | OPEN — found 2026-07-20, investigated further 2026-07-20 (2nd session): 4 separate isolated repros (resource-read, full TestCompiler+DynamicClassLoader reconstruction, classloading-race stress, pre-touch-Mockito-then-fork) ALL succeeded, matching HotSpot exactly — ruled out classloader delegation, pathing-jar Class-Path parsing, and retransform-shadowing. The real test is ALSO flaky in an unrelated way (assertion failures about management-context count) under heavy shared-host load; likely GC-timing-dependent rather than a clean classloader defect — not fixed |
| [`thread-dump-endpoint-jmx-threadinfo-fidelity.md`](thread-dump-endpoint-jmx-threadinfo-fidelity.md) | OPEN — found 2026-07-18; separate JMX diagnostic fidelity gap |
| [`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md) | OPEN — found 2026-07-17 |
| [`mongodb-dns-resolver-null-nameservers-npe-and-reactive-hang.md`](mongodb-dns-resolver-null-nameservers-npe-and-reactive-hang.md) | OPEN — found 2026-07-17 |
| [`nettyrsocketserverfactorytests-bindexception-os-error-10049.md`](nettyrsocketserverfactorytests-bindexception-os-error-10049.md) | OPEN — found 2026-07-17 |
| [`objectname-getkeypropertylist-ca-kp-array-npe-residual.md`](objectname-getkeypropertylist-ca-kp-array-npe-residual.md) | OPEN — found 2026-07-17 (residual of a FIXED sibling bug) |
| [`otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang.md`](otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang.md) | OPEN — found 2026-07-17 (hypothesis, unconfirmed — no thread dump  |
| `propertieslauncher-loader-path-ignored-wrong-app-launched.md` | **FIXED 2026-07-20** — moved to `propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md`; 32/32 `PropertiesLauncherTests` PASS against `dev@939f61817`. **See the next row** — a separate, unrelated regression re-breaks 15/32 on later `dev` |
| `propertieslauncher-jarloading-cluster-broad-regression-jit-halfgap-suspected.md` | **FIXED 2026-07-21** — moved to `propertieslauncher-jarloading-cluster-broad-regression-FIXED.md`. The non-JIT/JIT-independent loader regression is closed: `PropertiesLauncherTests` is 32/32 PASS in both modes; the 83-class loader sweep has identical JIT/no-JIT outcomes with no new regression. |
| `pemcertificates-clientauth-rustls-decrypterror.md` | **FIXED 2026-07-20** — moved to `pemcertificates-clientauth-rustls-decrypterror-FIXED.md`; `SSLContext.init` now resolves mTLS identity directly from the actual `KeyManager[]` it received (immune to an interleaved, unrelated `SSLContext.init` draining the old thread-local slot first) and picks the alias via the same `chooseClientAlias`-matching order instead of raw keystore-file order. Bonus fix: `TomcatReactiveWebServerFactoryTests` also went FAIL → PASS from the same root cause |
| `pulsar-propertiesmapper-timeunit-null-npe.md` | **FIXED** — moved to `pulsar-propertiesmapper-timeunit-null-npe-FIXED.md`; already landed on `dev` 2026-07-18 (`102b33b65`, an unrelated Spring Integration JMX fix) as a side effect — `unbox_wrapper` now widens an unboxed `Integer` to `long`/`float`/`double` instead of leaving it `Value::Int`-tagged. Confirmed 2026-07-21: full `module/spring-boot-pulsar` suite (6 classes, 117 tests) passes clean, JIT and `--nojit` |
| `quartzautoconfigurationtests-jdbc-jobstore-not-applied-FIXED.md` | FIXED — 2026-07-20; `java.util.Properties.putIfAbsent` had no native override (side-table miss), plus a residual `Channels.newReader`/`StreamDecoder` channel-read gap it was masking |
| `r2dbc-filteredclassloader-loadclass-override-bypassed-RESOLVED.md` | FIXED — 2026-07-18; reverified clean (JIT and `--nojit`) 2026-07-19 on current dev. Same root cause as the HTTP codec/JDBC/Micrometer-tracing `FilteredClassLoader` siblings |
| `rabbitautoconfigurationtests-cglib-enhance-hang-FIXED.md` | FIXED — 2026-07-20; not a hang, CPU-sampling proved genuine (if slow) progress — suite-timeout carve-out added (900s) plus a real residual bug fixed (`KeyManagerFactory`/`TrustManagerFactory.getInstance` now reject an unknown algorithm) |
| `reactor-netty-server-startup-hang.md` | **FIXED 2026-07-20** — moved to `reactor-netty-server-startup-hang-FIXED.md`; the hang itself was already fixed as a side effect of the 2026-07-18 `InterceptingExecutableInvoker` livelock fix. Running the class to completion exposed 12 residual failures; 9 fixed this session across three CratonVM bugs (`FileSystemProvider.getPath(jar:)` losing the jar/entry split, bind() errors never becoming typed `java.net.BindException`, a refused non-blocking connect reporting `connect()==true` instead of surfacing via `finishConnect()`) plus a `SunX509KeyManagerImpl`-compatible alias-ordering fix. Two residuals were filed separately: `sslWithPemCertificates` rustls DecryptError — FIXED 2026-07-20, [`h2c-priorknowledge-hpack-headerblock-decode-failure.md`](h2c-priorknowledge-hpack-headerblock-decode-failure.md) (still open) |
| `repeatablecontainers-method-cache-classcastexception-FIXED.md` | CLOSED 2026-07-20 — does not reproduce on current dev (4/4 clean runs); root cause never pinned, likely fixed as a side effect of unrelated collection/GC work between 07-17 and 07-20. The affected class now fails later, for an unrelated reason — see `mockresolver-dynamicclassloader-classnotfound-forked-testcontext.md` |
| [`resourcestests-trailing-slash-windows-path-error.md`](resourcestests-trailing-slash-windows-path-error.md) | OPEN — found 2026-07-17 (hypothesis, not confirmed against native `j |
| [`security-saml2-package-version-npe-and-x509key-unknown-algo.md`](security-saml2-package-version-npe-and-x509key-unknown-algo.md) | OPEN — found 2026-07-17 |
| [`servletcomponentscanintegrationtests-missing-registration.md`](servletcomponentscanintegrationtests-missing-registration.md) | OPEN — found 2026-07-17, not root-caused |
| `spring-boot-cloudfoundry-rerun-20260717-FIXED.md` | FIXED — 2026-07-19; skip-SSL-verification honored, `$Proxy` livelock resolved (side effect of the same TLS bridge fixes) |
| `spring-boot-cloudfoundry-mockwebserver-taskqueue-shutdown-FIXED.md` | FIXED — 2026-07-19; server-wrap TLS socket now has a bounded keep-alive read timeout so `MockWebServer.close()`'s 5s idle-queue wait no longer times out |
| [`spring-boot-configuration-processor-testcompiler-hang-cluster.md`](spring-boot-configuration-processor-testcompiler-hang-cluster.md) | OPEN — found 2026-07-17 |
| [`spring-boot-devtools-residual-fails-cluster.md`](spring-boot-devtools-residual-fails-cluster.md) | OPEN — found 2026-07-17 |
| `spring-boot-health-rerun-20260717-FIXED.md` | FIXED — 2026-07-20; `File.getUsableSpace()`/`getFreeSpace()`/`getTotalSpace()` were hardcoded `i64::MAX` stubs, plus a second stub on `FileSystem.getSpace(File,int)` exposed once Mockito's inline mock maker retransforms `java.io.File` process-wide; both now query real OS disk-space. Its 4 `CapturedOutput` classes were the already-tracked `capturedoutput-empty-console-cluster.md` residual, re-verified still passing |
| [`spring-boot-loader-classpath-url-enumeration-empty-cluster.md`](spring-boot-loader-classpath-url-enumeration-empty-cluster.md) | OPEN — found 2026-07-17 |
| `Archive`/`Launcher` classpath URL enumeration empty/wrong | **FIXED 2026-07-19** — verified 10/10 `JarFileArchiveTests`, 4/4 `WarLauncherTests`, 7/7 `ExplodedArchiveTests`, 5/5 `JarLauncherTests`; moved to `spring-boot-loader-classpath-url-enumeration-empty-cluster-FIXED.md`. Residual `propertieslauncher-loader-path-ignored-wrong-app-launched.md` also FIXED 2026-07-20, see `propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md` |
| [`spring-boot-loader-zipfile-close-invokespecial-native-bypass-npe.md`](spring-boot-loader-zipfile-close-invokespecial-native-bypass-npe.md) | OPEN — found 2026-07-17 |
| `spring-boot-restclient-residuals.md` (Issue A hang + Issue B `arg$1` reflection + `ObservationRegistry` residual) | **FIXED/CLOSED 2026-07-21** — Issue A already fixed 2026-07-18 (see the `InterceptingExecutableInvoker` cluster entry above); Issue B (`getDeclaredField("arg$1")` on a lambda-proxy class always threw `NoSuchFieldException`) fixed by synthesizing captured-variable fields from `capture_types`, 1-based-named (`arg$1`=capture 0) and non-synthetic (real HotSpot doesn't mark them `ACC_SYNTHETIC`, and AssertJ's `FieldUtils.readField` rejects synthetic fields); verified `HttpServiceClientAutoConfigurationTests` 7/7, `ReactiveHttpServiceClientAutoConfigurationTests` 7/7, `ReactiveOAuth2ResourceServerAutoConfigurationTests` 50/50; moved to `spring-boot-restclient-residuals-FIXED.md`. The `ObservationRegistry` `BeanDefinitionOverrideException` residual discovered once Issue A stopped masking it is now also FIXED — see below; all residuals of this doc are now closed |
| `observationregistry-conditionalonmissingbean-classpathexclusions.md` (`@ConditionalOnMissingBean` misses a user bean under `@ClassPathExclusions`) | **FIXED — confirmed 2026-07-21**, fix landed 2026-07-20 as a side effect of an unrelated commit (`65d738bb5`, loader-identity in no-receiver lambda/method-reference dispatch); re-verified 8/8 clean (`-Jit on`/`off`, repeated runs) plus a 50-class regression sweep across `spring-boot-restclient`/`spring-boot-webclient`/`spring-boot-security-oauth2-resource-server` with no regression to any previously-fixed class; moved to `observationregistry-conditionalonmissingbean-classpathexclusions-FIXED.md` |
| [`restclient-webclient-withoutjackson-cluster.md`](restclient-webclient-withoutjackson-cluster.md) | Bug A (`URLClassLoader.findClass` concurrent double-define race on `ConditionOutcome`, found evaluating `OnClassCondition$ThreadedOutcomesResolver`'s background thread pool racing the main thread through the same `ModifiedClassPathClassLoader`) **FIXED 2026-07-21** — new per-(loader,name) lock in `classloader.rs`'s `ucl_try_define_local_class`. Bug B (`RestTemplateBuilder`/related bean never registered, unmasked by Bug A's fix) still **OPEN**, not root-caused — see doc |
| Spring proxy layout-probe livelock blocking broad HttpClient consumers | **FIXED 2026-07-18** — JUnit adapter receiver admission now prevents the unrelated field probe; verified in JIT and `--nojit`; moved to `spring-proxy-layout-livelock-blocking-httpclient-consumers-FIXED.md` |
| `module/spring-boot-tomcat` 2026-07-17 rerun: 3 unrelated FAILs + throughput-wall HANGs | **FIXED/CLOSED 2026-07-19** — WAR-rooted `URLClassLoader` resource resolution fixed (narrow URL-format residual noted inline), Tomcat metrics MBean binding fixed upstream, `SslConnectorCustomizerTests` root-caused to the rustls CBC-cipher-suite gap below, since fixed; throughput wall unchanged (not a bug); moved to `spring-boot-tomcat-rerun-20260717-residuals-FIXED.md` |
| `RecordableServerHttpRequestTests.getRemoteAddress()` NPE + `WebFluxManagementChildContextConfigurationIntegrationTests` HANG | **FIXED 2026-07-19** — `InetSocketAddress(int)` now synthesizes a resolved wildcard address; `getfield`/`putfield` loader-aware field resolution + `MergedAnnotation$Adapt.isIn` cross-loader identity bridge closed the hang; verified 5/5 and 3x clean 4/5 completions respectively; moved to `spring-boot-webflux-residuals-FIXED.md`. New, unrelated residual discovered in current `dev` (same class, different/later hang) — see [`webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md`](webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md) |
| [`webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md`](webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md) | OPEN — found 2026-07-19, pre-existing `dev` regression unrelated to the fix above, not yet bisected |
| PEM private-key parse + PKCS12 keystore load (3 JCA/keystore gaps) | **FIXED 2026-07-19** — curve-aware XDH/EdDSA `KeyFactory`, PBES2 `SecretKeyFactory`, real SHA-256 PKCS12 MAC; moved to `ssl-pem-pkcs12-store-parse-failure-cluster-FIXED.md`. `SslConnectorCustomizerTests`, originally cross-filed here, turned out unrelated — see the CBC-cipher-suite row below |
| CBC-mode TLS 1.2 cipher suites unavailable (rustls backend) | **FIXED 2026-07-20** — found 2026-07-19; rustls's stock crypto providers never implemented CBC-mode cipher suites, so CratonVM vendored and patched rustls's TLS1.2 key schedule to add real `TLS_ECDHE_{RSA,ECDSA}_WITH_AES_{128,256}_CBC_SHA{256,384}` support, validated against a real OpenSSL interop handshake. `sslEnabledMultipleProtocolsConfiguration` now fails for a separate, unrelated reason: rustls has never implemented TLS 1.0/1.1 (a distinct, permanent protocol-version gap, not a cipher-suite one) |
| `SslMeterBinderTests` gauge count doubles after `SslBundle` update | **FIXED 2026-07-19** — PKCS#12 `Pkcs8ShroudedKeyBag` decrypt failure dropped the whole `PrivateKeyEntry` instead of keeping an encrypted placeholder, and cert chains were never extended past the leaf by issuer/subject DN matching; moved to `sslmeterbindertests-gauge-duplication-on-bundle-update-FIXED.md` |
| `StaticResourceJarsTests` JAR/URL-encoded-path resource lookup failures | **FIXED 2026-07-18** — `File(URI)` percent-decoding + `JarURLConnection`/`JarFile` caching and closed-state tracking; verified in JIT and `--nojit`; moved to `staticresourcejarstests-jar-url-handling-cluster-FIXED.md` |
| Thymeleaf Groovy layout-dialect `DecorateProcessor` constructor mismatch + missing `CapturedOutput` warning | **FIXED 2026-07-19** — `invokedynamic`'s generic `MethodHandle.invoke` bridge erased trailing `boolean` call-site args to `Integer`; preserved the real target descriptor and boxed `Z` args as `Boolean`; `CapturedOutput` half was already fixed by the project-wide Logger/LogFactory cluster; verified in JIT; moved to `thymeleaf-groovy-layoutdialect-cluster-FIXED.md` |
| `ThymeleafServletAutoConfigurationTests.createLayoutFromConfigClass` hang building a Groovy `MetaClass` | **FIXED/CLOSED 2026-07-21** — three independent bugs: GC `ReferenceQueue` field collision (fixed 2026-07-19), non-identity-stable `TypeVariable` for `Class.getTypeParameters()` (fixed 2026-07-20, `e426eadde`), and a silent regression of the `path-tostring-indy-stringconcat-dead-dispatch` fix below (the `vm_exec.rs` hunk went missing from `dev` sometime after 2026-07-19, re-added 2026-07-21). Full class now 27/27; moved to `thymeleaf-groovy-layoutdialect-metaclass-introspection-hang-FIXED.md` |
| `"literal:" + aPath` dead-dispatched to `Object.toString()` via `String.valueOf`/indy string concat | **FIXED 2026-07-19** — `String.valueOf(Object)`'s real `obj.toString()` call site never retargeted onto the receiver's actual class (CP-symbolic class `java/lang/Object` isn't interface/abstract); added a receiver-aware `toString()` check ahead of the resolved `class_name`; verified `ThymeleafReactiveAutoConfigurationTests` 21/21 (up from 20/21); moved to `path-tostring-indy-stringconcat-dead-dispatch-FIXED.md`. **Silently regressed and re-fixed 2026-07-21** — see the `metaclass-introspection-hang` entry above |
| [`tls-sslbundle-trust-validation-gap-cluster.md`](tls-sslbundle-trust-validation-gap-cluster.md) | OPEN — found 2026-07-17 |
| [`web-server-mockito-restub-no-op-cluster.md`](web-server-mockito-restub-no-op-cluster.md) | OPEN — found 2026-07-17 |
| `webclient-loopback-self-connect-timeout-os10060-cluster.md` | **FIXED 2026-07-20** — moved to `webclient-loopback-self-connect-timeout-os10060-cluster-FIXED.md`; root cause pinned to `SocketChannel.connect()`'s non-blocking deferred-failure path (`native-io/src/socket_channel.rs`) falsely reporting synchronous success, hiding a real connect failure from Netty until an untyped raw OS error surfaced later on write — fixed by already-merged `49d7834e9`. Verified via 4 independent suite-runner runs (88/88 test methods clean) |
| [`webmvc-error-forward-and-multiboot-timeout-cluster.md`](webmvc-error-forward-and-multiboot-timeout-cluster.md) | OPEN — found 2026-07-17 |
| [`webmvc-test-anonymous-tostring-override-not-dispatched.md`](webmvc-test-anonymous-tostring-override-not-dispatched.md) | OPEN — found 2026-07-17 (hypothesis for the dispatch gap; the format |
| [`webserversslbundletests-pkcs12-mac-verification-failure.md`](webserversslbundletests-pkcs12-mac-verification-failure.md) | OPEN — found 2026-07-17 |
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
`FIXED` rows above (their write-ups have since been archived out of this tracked directory).

Triaged the new CRASH/FAIL set from this rerun and filed 5 more docs (rows
above, dated 2026-07-16). Two were the **same newly-discovered, critical GC
regression** (`testengine-getid...`/`basiccontroller-stale-pointer...`) — a
brand-new young-GC pre-forwarding walk added the same day silently dropping
live objects from the forwarding set under certain free-list/TLAB-remnant
shapes. The fix (`fix/wildfly-cce0079-close-20260716`) merged into `dev`
while this investigation was still in progress; both docs were verified
FIXED the same session (rebuild + rerun confirmed
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
