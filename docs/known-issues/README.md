# Known issues вЂ” index & bug map

This folder collects CratonVM-only defects found while running upstream Java
suites. The docs had grown to describe the **same underlying bug from several
angles**; this index is the consolidated map. Read it first.

## 2026-07-22 H2 `cp500`/IBM500 charset FIXED

FIXED (moved to `docs/internal/fixed-suite-bugs/`):
[`bug-h2-charset-cp500-unsupported-FIXED.md`](../internal/fixed-suite-bugs/bug-h2-charset-cp500-unsupported-FIXED.md)
— `Charset.forName("cp500")`/`("IBM500")` threw `UnsupportedCharsetException`
because CratonVM's charset engine only curates a subset of the real JDK's
`jdk.charsets`-module extended charsets, and IBM500 (EBCDIC 500
International) wasn't one of them, breaking `org.h2.test.db.TestSetCollation`
(`testCp500Collator`) and `org.h2.test.unit.TestCharsetCollator`. Fixed by
adding IBM500 as a new single-byte codec in `native-api/src/charset.rs`
(canonical-name aliases, decode/encode, 256-byte table + reverse-lookup),
following the same pattern the existing IBM1047 codec uses. The 256-byte
table was captured directly from real JDK25's `sun.nio.cs.ext.IBM500` (not a
generic reference table) to match a genuine byte-0x15 NEL/LF ambiguity in
this codepage exactly. Verified: standalone `Charset.forName`/round-trip
probe, both affected H2 test classes pass (and reproduce on an unmodified
pre-fix baseline binary, confirming the A/B), zero regressions in
`cratonvm-native-api`/`cratonvm-native-builtins`'s combined 3236-test unit
suite. This is the second time this exact bug was documented — the first
doc was deleted by `b71e7402f` (2026-06-22 doc cleanup) without being fixed;
this closure is a real fix, not another cleanup deletion.

## 2026-07-20 CRITICAL core JIT/OSR bug FIXED: back-edge OSR silently re-executed loop iterations after an `invokedynamic` trap

FIXED (moved to `docs/internal/`):
[`jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md`](../internal/jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md)
— a `for` loop long enough to trigger back-edge OSR compilation, followed by
an `invokedynamic` call site (e.g. Java 9+ indy-based string concatenation,
as in `System.out.println("..." + x)`) in the same method, could silently
re-execute the loop's already-committed iterations with no exception —
e.g. an `ArrayList` built in a loop ending up with duplicate/extra elements.
Root cause: the indy call site's unconditional `UnreachedCode` deopt trap
reached the OSR-exit transfer (`transfer_osr_exit_into_live_frame`,
`vm/src/runtime/interpreter.rs`) with reconstructed state the transfer
couldn't map (an `Unsupported` local from a coarse whole-method slot-reuse
classification, and — always — `Unsupported` operand-stack values at the
indy site itself); the transfer's "safe reject" fallback then resumed the
interpreter from the STALE pre-OSR pc/locals, silently re-running everything
OSR had already executed. Fixed by (1) tolerating an unmappable LOCAL slot
in the transfer instead of rejecting it whole (leaving that slot's live
value untouched — safe per the JVM verifier's definite-assignment rule) and
(2) banning OSR compilation for any method containing `invokedynamic`
(new RBC.7, mirroring the existing `athrow` ban in `compile_osr_artifact`).
Not Spring-specific — a core VM/JIT correctness bug; discovered as a
byproduct of the `repeatablecontainers-method-cache-classcastexception`
investigation the same day. Verified across the full originally-reported
threshold range (100–15000 iterations) plus a value-level diagnostic
(no duplicated index, not just a correct final count); `cargo test --release
-p cratonvm-vm --lib` shows no new failures.

## 2026-07-20 Spring suite genuine-bug list reconfirmed: 107/177 fixed, 70 remain

Reran the 263 previously-non-passing classes from the 2026-07-17 full-suite
triage on a fresh `dev` merge (`8719dca85`, ~3 days later). **107 of the 177
confirmed genuine bugs are now fixed**, including 24/26 of the systemic JMX
cluster and the entire 25-class `test.context.jdbc.*` cluster. 70 remain
open — notably the AOT/TIMEOUT cluster (still fully hung, grew to 16
classes), the HTTP JSON message-converter cluster (all 8 still failing), and
`scheduling.concurrent.*` (all 4 still failing).
[`CRATONVM-SPRING-GENUINE-BUGLIST.md`](CRATONVM-SPRING-GENUINE-BUGLIST.md)
has been trimmed to just the 70 still-open classes.

## 2026-07-17 Spring suite: full 2912-class run, 177 genuine bugs fully HotSpot-triaged (complete replacement of prior partial docs)

Ran the **complete** Spring Framework suite (all 2912 classes, no filtering)
on dev `213d93ea` — 91% OK (2649/2912). Cross-referenced every one of the 263
non-OK classes against HotSpot (134 via the existing `hs516_final.tsv`
baseline, the remaining 129 via a fresh targeted HotSpot rerun) — nothing
left unclassified. Result: 86 environmental (73 EMPTY + 13 FAIL, both match
HotSpot exactly) + **177 genuine CratonVM bugs**. Notable: the entire `jmx.*`
module (26 classes) fails — confirms it was never actually fixed, despite an
earlier signature-mismatch fix; `test.context.jdbc.*` (30 classes) fails
100% uniformly behind Spring's own failure-threshold circuit breaker,
masking the real root cause; the AOT bean-registration TIMEOUT cluster from
the prior doc is unchanged. See
[`CRATONVM-SPRING-GENUINE-BUGLIST.md`](CRATONVM-SPRING-GENUINE-BUGLIST.md).
This **replaces** the prior `-125`/`-159` lineage and the
`CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md` doc, both of which only
covered a 516-class subset ~986 commits behind this run — both retired to
`docs/internal/spring/`.

## 2026-07-16 WildFly boot CCE family ROOT CAUSE FIXED — moving young GC's object-start walk truncated at the first TLAB gap, mass-dangling references (was misattributed for weeks as "register-invisible JIT roots" / per-site missed pins)

FIXED (full writeup): [`wildfly-cce0079-young-start-set-truncation-FIXED.md`](../internal/fixed-suite-bugs/wildfly-cce0079-young-start-set-truncation-FIXED.md)
— the `WFLYCTL0079` / `ClassCastException: java.lang.Object cannot be cast to X` family during
`parallel-extension-add` (`AttributeAccess`/`AttributeDefinition`/`Comparable`/`Function`/`Map`/…
cast targets), the `via_pin=true` mystery, and a swath of "silent wedge" boot failures all traced
to ONE defect: `collect_garbage_inner`'s moving-path `young_object_starts` walk `break`'d at the
first free-list/TLAB/GAP-filler gap (warning present in 100% of baseline logs) and
`forward_object` then refused to evacuate every young object above the breakout — for precise
roots and native pins included. Fixed with a gap-aware walk + a skip-cycle fail-safe (measured:
diverting to the non-moving sweep instead reclaims live objects on the precise-root path —
HIB-CV-22/32/33). Standalone CCE rate 0/14 post-fix vs ~50% baseline. Landed alongside: ~35
audited Family-1 stale-at-store fixes (native-collections TreeMap/PriorityQueue/ArrayDeque/
LinkedList/COWAL/HashSet-bulk/LinkedHashMap-eviction + lookup family), XNIO conduit/worker
fixes (listener dispatch, channel-alloc registry keys), DataInput/OutputStream fixes, an
always-on RETURN-value `load_and_forward` healing barrier at the native-call funnel, and new
diagnostics (`CRATONVM_DBG_STALE_OBJREF_CYCLES` quarantine ring, `CRATONVM_DBG_CCE_BT`,
store-funnel stale-value checks). A narrow domain-no-JIT long-tail residual remains OPEN in
[`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`](wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md)
(2026-07-16 section); the JIT SIGSEGV bucket stays with the SB-CRASH-04/precise-maps roadmap.
Timeline correction (merge-time finding): the truncating walk itself was introduced the same
morning by `1c4aaa06`, so the 100%-rate collapse was a same-day regression amplifier on top of
the older lower-rate family (which the ~35 pin fixes + the return barrier address); `fb15be63`
independently landed the GAP-filler stride portion — this branch adds the free-list/TLAB merge,
walk-completeness tracking, and the skip-cycle fail-safe on top. Details in the FIXED writeup.

RETIRED with the same wave:
[`wildfly-domain-hc0053-server-inventory-timeout-RESOLVED.md`](../internal/fixed-suite-bugs/wildfly-domain-hc0053-server-inventory-timeout-RESOLVED.md)
— the multi-session WildFly domain-boot record (inventory transport → StreamDecoder →
async-future/XNIO AB-BA deadlock → blocked on this CCE family) captured its final closing
artifact: BOTH managed servers reaching `WFLYSRV0025` in one clean run (`DM_001`: server-one
70.1s, server-two 84.2s, zero failure markers in the domain console log).

## 2026-07-15 Keycloak `WelcomePageTest` zipfs `Files.copy` bug FIXED (two stacked path-layout bugs); teardown hang re-verified NOT reproducing

FIXED (moved to `docs/internal/fixed-suite-bugs/`): [`zipfs-files-copy-wrapped-path-FIXED.md`](../internal/fixed-suite-bugs/zipfs-files-copy-wrapped-path-FIXED.md)
-- closes item 3 of [`keycloak/welcomepagetest-stream-spliterator-zipcopy-residuals-20260715.md`](keycloak/welcomepagetest-stream-spliterator-zipcopy-residuals-20260715.md)
("`Files.copy()` from a non-default `FileSystemProvider` path fails"), which blocked Quarkus's
`ZipUtils.unzip()` (used by `DistributionKeycloakServer.createInstallation()` to extract the Keycloak
distribution for every `tests/base` integration test that needs a running server). Two independent
`native-builtins/src/phases_late.rs` bugs stacked: (1) `Files.copy`'s native didn't classify a
jarfs-encoded *source* path, only the destination; (2) `p57_read_path()` silently mis-read a Quarkus
`PathWrapper` decorator Path (used by `ZipUtils`'s `ignoreFileWriteability` before every zip mount) as
an empty string, which made the zip mount silently fall back to the *real host filesystem root* --
`Files.walkFileTree` then tried to copy the entire host disk into the extraction target. Fixed +
merged to `dev`: `e38d6f60`/`90cc7e73` (bug 1), `a43436fc`/`882395cd` (bug 2). With both fixed, the
Keycloak 26.6.1 test server now boots successfully under `WelcomePageTest`, and the previously-reported
~27-minute post-test-completion teardown hang was re-run end-to-end and did NOT reproduce (process now
exits cleanly ~183s after starting, well under a second after the last test method finishes). The tests
themselves still fail for unrelated, already-tracked reasons (item 2's Selenium/Stream bug, and a newly
observed Maven artifact-resolution failure) -- see the known-issues doc's 2026-07-15 update section for
detail.

## 2026-07-15 `WFLYCTL0079` (any extension) during `parallel-extension-add`: generalized to the existing `AttributeAccess` CCE doc; "JIT required" DISPROVED; one real site FIXED, residual re-characterized

With the 2026-07-14 ObjectName fix below and the prior session's stale-`ObjectRef` fixes in place,
WildFly standalone boot progresses well past `parallel-extension-add` into "Building security domain"
before hitting `WFLYCTL0079: Failed initializing module org.wildfly.extension.io` (or, non-deterministically,
almost any other extension). Investigation found this is **the same bug** as
[`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`](wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md)'s
`ClassCastException: java.lang.Object cannot be cast to X` family, just generalized: repro batches hit it
against `org.wildfly.extension.elytron`, `org.jboss.as.jaxrs`, `org.wildfly.extension.undertow`,
`org.jboss.as.clustering.infinispan`, and `org.wildfly.extension.io` itself (twice), with cast targets
`AttributeAccess`, `AttributeDefinition`, `Comparable`, `RegistrationPoint`, `CapabilityRegistration`,
`Predicate`, and an `AttributeAccess$Flag[]` array — confirming the originally-reported module name is
circumstantial (whichever of the ~37-42 concurrent `parallel-extension-add` worker threads reads a
just-corrupted address first), not diagnostic. That doc's own "JIT is required" conclusion is **disproved**:
the identical crash reproduces under `--nojit` (4/8 attempts). One genuine, narrow contributing site was
found and FIXED —
[`../internal/fixed-suite-bugs/wildfly-invoke-virtual-lambda-sam-compat-stale-locals-FIXED.md`](../internal/fixed-suite-bugs/wildfly-invoke-virtual-lambda-sam-compat-stale-locals-FIXED.md):
`vm/src/vm/vm_exec.rs::invoke_virtual`'s lambda-dispatch decision point read `receiver`/`args` again,
unpinned, after its own `.filter()` predicate's `lambda_args_sam_compatible` call (which can trigger class
loading) — fixed by pinning both across that window. Verified via `cargo test -p cratonvm-vm --lib`
(2202 passed / 9 pre-existing `--release`-only failures, identical before/after via `git stash`), zero
regressions. **Does not close the residual**: matched before/after repro batches show the same overall
`WFLYCTL0079` rate (5/12 both). A live diagnostic (temporary instrumentation, not landed) proved the
remaining stale reads go through the codebase's existing pin-protection path (`via_pin=true`) and are
*still* stale — ruling out "yet another missed-pin site" and pointing instead at a cross-thread
GC-root-visibility/timing race across WildFly's ~37-42 concurrently-executing worker threads, a
meaningfully different (though related) characterization than the original doc's JIT-only
`SB-CRASH-04` attribution. See that doc's own 2026-07-15 follow-up section for the full evidence chain;
still OPEN, deliberately not further patched (deep GC/threading infrastructure work).

## 2026-07-14 Stream/ArrayList heap corruption under extreme small-heap GC pressure - FIXED 2026-07-16

The 24-thread 32 MB Stream/ArrayList repro is fixed and archived at docs/internal/fixed-suite-bugs/stream-arraylist-gc-pressure-heap-corruption-FIXED.md. The closure combines an old-to-young remembered-set fallback, terminal-worker STW publication, exact containment for conservative interior roots, a lazy-Stream pin, and a fail-closed non-moving JIT-active young-GC policy. Azure validation: two JIT and two no-JIT runs all reported RESULT=OK, plus 791 of 791 GC unit tests.

## 2026-07-14 WildFly standalone boot ObjectName `_ca_array` NPE FIXED (100% boot blocker, open since 2026-07-10's "Bug 3a")

- FIXED (moved to `docs/internal/fixed-suite-bugs/`): [`wildfly-standalone-boot-objectname-ca-array-npe-FIXED.md`](../internal/fixed-suite-bugs/wildfly-standalone-boot-objectname-ca-array-npe-FIXED.md) -- bisected regression (introduced by `d8092acb`, the same "fix-tests-real-jdk-contracts" commit responsible for the `String.getBytes()`/`java.util.Properties`/JMX-native-surface regressions in this file) that blocked 100% of WildFly-standalone-boot attempts on real JDK25, on the very first JMX MBean registration. Root cause: `d8092acb` correctly stopped `MBeanServerFactory.createMBeanServer` from being unconditionally shadowed by a synthetic server in real-JDK mode, which let real bytecode reach `Repository.addNewDomMoi` -> `ObjectName.getCanonicalKeyPropertyListString()` for the first time -- a method never natively covered against the synthetic 1-field `ObjectName` model (already known and deliberately left unfixed as "Bug 3a" in `managerwebapp-deploy-bare-assertion-FIXED.md`, 2026-07-10, when the synthetic-server shadow was still masking it). Fixed by adding `getCanonicalKeyPropertyListString`/`isPattern`/`isDomainPattern`/`isPropertyPattern`/`isPropertyListPattern` natives derived from the same canonical-string text model the rest of `ObjectName`'s natives already use. Note: a concurrent same-day fix below (`6a0eedd8`) independently re-masks `getPlatformMBeanServer()` (fixing its own, broader JMX-native-surface regression), so the *specific* WildFly boot path no longer exercises this fix either -- but the underlying `ObjectName` defect is now genuinely closed, not just re-masked, closing Bug 3a for good.

## 2026-07-14 (cont'd) String.getBytes(), Locale bootstrap, and HttpExchange URI regressions FIXED

- FIXED (moved to `docs/internal/`): [`string-getbytes-empty-real-jdk-mode-FIXED.md`](../internal/string-getbytes-empty-real-jdk-mode-FIXED.md) -- same failure class as the `java.util.Properties` fix (`f62d2073`): `register_real_charset_natives` (`native-builtins/src/charset.rs`) never set its own registry category, so its real-JDK-mode call sites inherited the default `SyntheticStub` and got silently dropped by `d8092acb`'s hardening. Fixed by wrapping the whole function body in `with_category(Bridge, ...)`. This was also the true root cause of the `com.sun.net.httpserver.HttpServer` "always empty body" symptom noted in the URLClassLoader fix above.
- FIXED (moved to `docs/internal/`, found already fixed on `dev` by a concurrent session): [`locale-real-jdk-bootstrap-noclassdeffounderror-FIXED.md`](../internal/locale-real-jdk-bootstrap-noclassdeffounderror-FIXED.md) -- same root mechanism, fixed via `f62d2073`'s `java.util.Properties` bridge-pinning (the `Locale`/`BaseLocale`/`StaticProperty` chain bottoms out in the same `System.getProperties()` read that fix restored).
- FIXED (moved to `docs/internal/`): [`httpserver-exchange-requesturi-getpath-empty-FIXED.md`](../internal/httpserver-exchange-requesturi-getpath-empty-FIXED.md) -- `HttpExchange.getRequestURI()` was writing the request target into a guessed synthetic `URI` slot (real-JDK slot 0 is `scheme`), leaving `toString()`/`getPath()` empty. It now uses the shared field-name-safe URI constructor; a live server probe verified the full target, decoded/raw path, and query.

## 2026-07-14 TestClassServerTest URLClassLoader isolation FIXED; severe new String.getBytes() regression found

- FIXED (moved to `docs/internal/`): [`keycloak-testclassserver-invalidpackage-classnotfound-FIXED.md`](../internal/keycloak-testclassserver-invalidpackage-classnotfound-FIXED.md) -- root cause was the same underlying defect as `spring-boot-probe-sweep/SBR-14-urlclassloader-parent-null-bypassed.md`: a null-parent `URLClassLoader` never consulted its own URL/HTTP classpath at all, resolving through CratonVM's flat global class store instead (breaking isolation AND making `testInvalidPackage`'s expected `ClassNotFoundException` never fire). Fixed in `native-builtins/src/classloader.rs`/`classloader_real.rs` (defer-to-`findClass` gate now covers bare `URLClassLoader`, not just subclasses) plus a genuine HTTP(S) fetch path added for URLClassLoader entries (`http_client.rs`). Verified via an isolated A/B repro against a real external HTTP server; the literal upstream test still can't run end-to-end due to the new bug below.
- 🔴 NEW, severe: [`string-getbytes-empty-real-jdk-mode.md`](string-getbytes-empty-real-jdk-mode.md) -- `String.getBytes()` (all overloads) returns an empty byte array in real-JDK mode, confirmed pre-existing (present on unmodified `dev` HEAD, not introduced by the fix above). Suspected fallout from the same-day commit `d8092acb`'s new synthetic-native-stub-dropping hardening silently dropping a genuine bridge native that was never re-categorized. Broad blast radius suspected (anything doing String-to-bytes: I/O, hashing, HTTP bodies) -- likely under-detected because failures land on higher-layer symptoms. Not yet fixed.

## 2026-07-14 Spring Boot `crashfail-20260714` rerun; 9 new bug clusters filed under `springboot/`

Full-suite rerun after the 7 clusters from 07-11/07-13 were fixed (see
[`springboot/README.md`](springboot/README.md) for the full table). While the
8-shard run was still in progress, triaged the CRASH set (8 fatal
process-aborts) and the clearest FAIL log-signature clusters, dispatching 8
parallel investigation agents. Two crashes have precise, high-confidence root
causes with concrete fix directions:

- [`springboot/charbuffer-order-missing-native-idn-clinit-cluster.md`](springboot/charbuffer-order-missing-native-idn-clinit-cluster.md) — `CharBuffer.order()` has no native registration; poisons `java.net.IDN`'s `<clinit>` for the rest of the process on first use (6 FAIL classes + 1 fatal CRASH).
- [`springboot/structured-logging-map-entry-getkey-lambda-dispatch-precedence.md`](springboot/structured-logging-map-entry-getkey-lambda-dispatch-precedence.md) — `Map.Entry::getKey`/`getValue` method references over a synthetic wrapper entry resolve to the wrong native override (interface-level generic beats the wrapper's own delegating native); confirmed with a standalone repro (5 classes).
- [`springboot/applicationcontextrunnertests-lazy-cglib-classnotfound-crash.md`](springboot/applicationcontextrunnertests-lazy-cglib-classnotfound-crash.md) — Spring's `@Lazy`-injection CGLIB proxy naming (`$$SpringCGLIB$$`) isn't recognized as a recoverable classloading miss, so an expected `ClassNotFoundException` escapes as an internal error and aborts the whole process (3 fatal CRASH classes; likely affects `@Lazy` injection broadly, not just these 3).

The remaining five are OPEN with strong, evidence-backed hypotheses not yet confirmed by live bisection: `jit-dispatch-depth-guard-shallow-stackoverflow-cluster.md`, `comparable-classcast-lambda-proxy-unknown-class.md`, `collectionbindertests-classcast-testdescriptor-crash.md`, `jsonvaluewritertests-nesting-depth-guard-stack-overflow.md` (a genuine native `EXCEPTION_STACK_OVERFLOW`, not a caught Java one), and `reactor-nettyhttpclient-httpclientsecure-null-provider-crash.md`. The Logback `LoggerContext` final-field cluster was fixed 2026-07-15 by restoring real constructor invocation; its archive is [`internal/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md`](../internal/fixed-suite-bugs/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md). None overlap with previously-retired clusters.

## 2026-07-13 WildFly `AttributeAccess` CCE confirmed as register-invisible-JIT-root family; NEW "Family 1" stale-ObjectRef residual found alongside it

- 🔴 NEW: [`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`](wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md)
  — follow-up to the `AttributeDefinition` CCE fix (`70154861`)'s flagged residual: `ClassCastException:
  java.lang.Object cannot be cast to org.jboss.as.controller.registry.AttributeAccess`, also during
  `parallel-extension-add`. Confirmed from source (not a fresh live capture — see below) as an occurrence
  of the already-tracked, currently-OPEN "register-invisible JIT root" family (`SB-CRASH-04`'s residual in
  the default-on precise-JIT-oop-map machinery): `jit_typecheck_resolve`'s fast path (the path an
  already-loaded class like `AttributeAccess` takes) makes zero GC-triggering calls, so staleness must
  originate upstream of the checkcast helper; and Generational's young collector is confirmed (from source,
  not aspirationally) to never relocate objects while any JIT frame is active, structurally excluding the
  classic "moving GC left a dangling pointer" mechanism for this JIT-required bug — leaving the non-moving
  sweep's marking phase missing a live root at a cooperative safepoint as the only mechanism consistent with
  the symptom, matching three independent 2026-07-10 occurrences of the same family (DoHead,
  `TestSwallowAbortedUploads`, `TestAccessLogValve`). Not fixed, per this family's established
  document-don't-speculatively-patch policy. Live reproduction of the CCE itself was inconclusive this
  session — the host was under heavy external CPU contention (confirmed via `wmic`/`ps`), and a **separate,
  real "Family 1" stale-`ObjectRef`-across-GC bug** (unrelated native code holding a raw `ObjectRef` across a
  GC-triggering call, unpinned) fired instead in 9/20 diagnostic (`CRATONVM_DBG_STALE_OBJREF=1`) attempts,
  immediately as `parallel-extension-add`'s worker threads spin up — a fresh occurrence found on a build
  forked *after* `docs/internal/wildfly-parallel-boot-stale-objectref-residual.md`'s own "Status: FIXED"
  date, not yet root-caused to an exact call site (Windows backtrace symbolication did not resolve despite a
  matching `.pdb`), flagged as its own separate follow-up (see the new doc's "Separate finding" section).

## 2026-07-13 Keycloak quarkus/runtime SmallRye Config resolution mismatches FIXED (3/4); PicocliTest hang split out as separate open bug

- FIXED (moved to `docs/internal/`): [`fixed-suite-bugs/keycloak-quarkus-runtime-config-resolution-mismatches.md`](../internal/fixed-suite-bugs/keycloak-quarkus-runtime-config-resolution-mismatches.md) вЂ” landed on `dev` via commit `10a561f21` earlier the same day this doc's investigation resumed. 3 of 4 original symptoms confirmed fixed by rerun: `DatasourcesConfigurationTest` (host-env-leak into `propagatedPropertyNames`, plus the interceptor-context `NoSuchMethodError`), `TracingConfigurationTest` (hardcoded-wrong `isTracingEnabled` native stub removed), `IgnoredArtifactsTest`. A narrower residual remains OPEN and is tracked inline in that doc rather than as a separate file: `ConfigurationTest::testDatabaseProperties` intermittently (~80% of runs) throws a `ClassCastException: Object cannot be cast to String` from `SmallRyeConfig$ConfigSources$PropertyNames.latest()` вЂ” confirmed genuinely racy (diagnostic instrumentation that merely reads extra class-name info per stream iteration made it disappear 6/6 vs failing 5/5 without it), not reproducible in a clean standalone repro (needs accumulated state from ~72 prior tests in the class), root cause not pinned down (leading suspect: `PropertyMappingInterceptor.iterateNames()`'s `mappersWithoutValues.stream()...` combined with `hasInferredValue`'s reentrant `context.restart()` call, but not confirmed).
- FIXED (moved to `docs/internal/`, 2026-07-14): [`fixed-suite-bugs/quarkus-runtime-picocli-arggroupspec-synopsis-hang-20260713-FIXED.md`](../internal/fixed-suite-bugs/quarkus-runtime-picocli-arggroupspec-synopsis-hang-20260713-FIXED.md) — the full 107-test `PicocliTest` class now runs to completion with no hang (verified twice, including at the true current `dev` tip), and the doc's own exponential-fan-out theory (N possibly 30-40) is refuted by direct measurement (real N=3). A separate, distinct correctness issue (26/107 assertion failures, newly visible now that the class runs to completion) is being triaged independently and is not part of this closure.
## 2026-07-13 DoHead 64-class family sweep GREEN + new sporadic Thread.start() finding

Full-family validation of all 64 `TestHttpServletDoHeadInvalidWrite*`
classes (18,432 tests) on current dev: 58/64 clean, 6 at 287/288, zero
GC/dispatch/socket cluster signatures (details appended to
[`dohead-jit-heap-corruption-register-invisibility-FIXED.md`](../internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md)).
The sweep A/B also confirmed the STW-takeover bracketing fix (`945e44920`)
as the root cause of the long-standing cross-suite ~300s
`SocketTimeoutException` flake family (33 hits in the 07-12 baseline в†’ 0).
One NEW low-rate residual found and filed:
[`tomcat/threadpoolexecutor-prestart-illegalthreadstate-sporadic.md`](tomcat/threadpoolexecutor-prestart-illegalthreadstate-sporadic.md)
(`Thread.start()` on a freshly constructed endpoint worker sporadically
throws `IllegalThreadStateException`, ~1/5000 Tomcat boots).

## 2026-07-13 s2 ByteBuffer real-JDK direct-buffer/interop gaps RETIRED (all 7 items fixed; compound-file residual refuted)

- FIXED/RETIRED (moved to `docs/internal/`): [`fixed-suite-bugs/s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md`](../internal/fixed-suite-bugs/s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md) вЂ” every remaining item closed in one pass via the doc's own suggested single storage-view helper (`servlet.rs::s2_bb_storage`: heap array + real `offset` base OR direct native address). (2) `equals`/`hashCode`/`compareTo` now storage-aware (and `hashCode` now iterates backward like real `Buffer.hashCode` вЂ” the old forward loop diverged from HotSpot on every 2+-byte buffer); (3) `slice()`/new `slice(II)`/`duplicate()`/`asReadOnlyBuffer()`/typed views are now genuine ALIASING views over heap (array-base `offset`, honoured by every accessor, returned by `arrayOffset()`) or direct (`address`) storage вЂ” the old copies meant writes through a slice never reached the parent; `isDirect`/`isReadOnly`/`hasArray`/`array` answer from storage/flags and read-only mutation throws the new `ReadOnlyBufferException`; (4) the `ChecksumIndexInput.getChecksum()` HotSpot divergence was NOT a ByteBuffer bug: a synthetic-era native override of Lucene's `BufferedChecksumIndexInput.getChecksum()` re-read the file and recomputed CRC over `length-8` bytes whenever position was within 8 bytes of EOF (right answer ONLY in CodecUtil's footer idiom вЂ” which is why self-verifies passed; ProbeNIOFS2's full-file read got CRC(first 4992 bytes) = 170114997, verified arithmetically); it now returns `digest.getValue()` like the real bytecode; (5) the `ByteBuffersDirectory` reflective `NoSuchMethodException: <init>` was `Class.asSubclass` never throwing `ClassCastException` вЂ” Lucene's `newFSDirectory` USES that CCE to fall back to a random FSDirectory for a non-FS forced `tests.directory`; asSubclass now enforces the subtype check; (6) `ByteOrder.toString`/`equals` decoded real `name`-String field 0 as an int (everything printed BIG_ENDIAN) and `nativeOrder()`/static getters allocated fresh synthetics per call (identity comparisons always false) вЂ” all now return/decode the canonical real statics, and `buffer.order()` ensures `ByteOrder.<clinit>` ran; (7) the 7-vs-53 JIT iteration divergence does not reproduce: 53/53 PASS JIT-on and `--nojit`. **The 2026-07-13 "compound-file copyBytes loses 46 bytes" residual (`remaining=-30, expected=16, fp=2517`) is NOT a dev bug**: the exact forced-NIOFS `testMultiClose` repro passes on the dev tip (3/3 runs) and on the fixed build; it reproduces ONLY under the uncommitted ~460-line `allocateDirect` rework left in worktree `cratonvm-s2-bytebuffer-real-direct-20260712` (verified by running that session's binary against the identical command). Verified: probe battery vs HotSpot jdk25 (buffer semantics incl. aliasing + JDK-reference hashCode, CRC32 paths, checksum-input bulk/per-byte/mixed/footer-idiom, ByteOrder identity, asSubclass, reflective Directory shapes), `testMultiClose` PASS under forced NIOFSDirectory AND forced ByteBuffersDirectory, full `ES93FlatBFloat16VectorFormatTests` 53/53 JIT-on + `--nojit`, regression sweep (`ES813FlatVectorFormatTests` 53/53, `ES815BitFlatVectorFormatTests` 6/6, `ES93HnswBFloat16VectorsFormatTests`), zero unit-test regressions in `cratonvm-native-builtins`/`cratonvm-types`.

## 2026-07-13 `OnClassCondition` NPE-cast-to-`String[]` cluster FIXED (largest single Spring Boot cluster, 75 classes)

- FIXED/RETIRED (moved to `docs/internal/`): [`springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md`](../internal/fixed-suite-bugs/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md) вЂ” `annotation_element_to_java_typed`'s `Class`-typed-element resolution returned a bare Java `null` instead of a deferred `TypeNotPresentException` sentinel for an unresolvable class outside the (rare) classloader-isolation path вЂ” the common case for `@ConditionalOnClass(SomeOptionalClass.class)`. Spring's own `@ConditionalOnClass` machinery is specifically written to catch that exception; the `null` instead let a `classValuesAsString` conversion NPE, surfacing as `OnClassCondition.addAll`'s `ClassCastException: java.lang.NullPointerException cannot be cast to [Ljava.lang.String;` across 75 classes. Fixed by building the sentinel in that path too. Verified against all 75/75 originally-affected classes вЂ” zero residual. Along the way, applying the fix appeared to expose an unrelated heap-corruption bug in 3 classes; that turned out to already be independently fixed on `dev` the same day (`e7e3bb91f`, for a Flyway/CGLIB SIGSEGV) вЂ” this fix's new code path was just exercising that same pre-existing `read_string`-misidentifies-arrays-as-Strings bug far more often. The separate Brave summary-printing residual is now also **FIXED/RETIRED**: [`springboot/brave-baggagefields-classcast-summary-printing-FIXED.md`](../internal/fixed-suite-bugs/springboot/brave-baggagefields-classcast-summary-printing-FIXED.md).

## 2026-07-12 Spring Boot HANG-rerun follow-up: 2 more clusters filed (both now fixed)

Rerunning the original 150 HANG classes at 5x timeout (1500s; see
`springboot/README.md` "HANG-rerun follow-up") surfaced two new findings:

- **FIXED 2026-07-12**: [`../internal/flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md`](../internal/flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md) вЂ” `FlywayAutoConfigurationTests` SIGSEGVs after ~3.5 minutes of repeated heap-corruption warnings at the same three stable addresses (the already-tracked HIB-CV-32 family). Root cause: `NativeContextImpl::read_string` identified a String from the heap header's class ID alone, but CratonVM reference arrays store their **component** class ID in that header, so a `String[]` was wrongly accepted as a `java/lang/String` вЂ” `read_string` now requires `ObjectKind::Object` first. This same fix also turned out to resolve the heap-corruption residual from the `OnClassCondition` fix above.
- **FIXED/RETIRED 2026-07-13**: [`testcompiler-annotation-classes-not-found-cluster-FIXED.md`](../internal/fixed-suite-bugs/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md) вЂ” all 7 `spring-boot-configuration-processor` classes now pass (94/94 tests). The complete fix made forced-native `JavacFileManager.list` GC-safe while streaming the full JRT package inventory, then preserved application-provided `URLStreamHandler` semantics for Spring's generated in-memory `resource:` URLs.

Most of the rest of the 51 reclassified-to-FAIL classes overlap with
already-filed clusters (`OnClassCondition` NPE, destroy-method resolution,
`MemoryAccessOption`) вЂ” they just needed more wall time to reach the
already-known failure instead of timing out first.
## 2026-07-12 ES vector-codec footer/checksum mismatch FIXED вЂ” s2 ByteBuffer bulk get/put self-copy on direct receivers

- FIXED/RETIRED (moved to `docs/internal/`): [`elasticsearch-suite/ES-FAIL-FAMILY-20260709-vector-codec-footer-mismatch-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-FAIL-FAMILY-20260709-vector-codec-footer-mismatch-FIXED.md) вЂ” the s2 synthetic `java/nio/ByteBuffer` native family's bulk `get([BII)`/`get([B)`/`put([BII)`/`put([B)` accessors (`native-builtins/src/servlet.rs`) located backing storage with `s2_bb_arr(ctx, this).unwrap_or(src/dst)`; on a genuine real-JDK `DirectByteBuffer` receiver (no heap array, `s2_bb_arr` returns `None`) this made the source/destination array copy into or out of *itself* вЂ” a silent self-copy that dropped every write and returned stale/zero data on every read while still reporting success and advancing position. Same shape as the already-fixed (2026-07-10) `put(Ljava/nio/ByteBuffer;)` bug, on the `byte[]`-bulk accessors instead. `IOUtil.read` routes every buffered `FileChannel` read through a temporary direct buffer and then bulk-`get`s it into a `byte[]`, so this is exactly how Lucene's index footer/checksum bytes came back as zero (`actual footer=0`). Corrected an initial hypothesis along the way: `s2_bb_get_byte`/`s2_bb_put_byte` (the single-byte primitives every scalar/typed accessor funnels through) were suspected of breaking `getInt`/`putInt`/`getLong`/`putLong`/etc. too, but a debug-instrumented build proved those scalar accessors are overridden by the *concrete* `java.nio.DirectByteBuffer` class in real JDK and run as real bytecode (using the buffer's real `address` field) вЂ” CratonVM's force-dispatch only matches the literal `java/nio/ByteBuffer` class name, so it never intercepts them on a genuine direct buffer. Only the `byte[]`-bulk methods, which `DirectByteBuffer` does not override, stay force-dispatched to the broken native. Fixed by adding the same direct-address (`s2_bb_direct_addr` + `copy_from/to_native_memory`) fallback the 2026-07-10 fix used, to both the 4 bulk methods (throwing `IllegalStateException` on a failed native-memory access, matching the existing pattern) and defensively to `s2_bb_get_byte`/`s2_bb_put_byte` (panic-free benign fallback, since those helpers have no `MethodCallResult` to throw through вЂ” they're still reached by slices/duplicates of a direct buffer and typed views over one). Verified: `ES813FlatVectorFormatTests`/`ES93FlatVectorFormatTests`/`ES93FlatBFloat16VectorFormatTests`/`ESNextOversamplingMetaTests` all pass matching HotSpot (53/106/53/53 tests) under both JIT-on and `--nojit`; zero regressions in `native-builtins`'s 2985-test unit suite (the 3 tests that differed pre/post-fix are confirmed pre-existing parallel-harness flakiness, passing individually) and the `vm` crate's 26-test `ByteBuffer`/`CharBuffer`/`IntBuffer`/`LongBuffer` interpreter suite. Does NOT fix the separate, still-open `ChecksumIndexInput.getChecksum()` divergence from HotSpot (item 4 of [`s2-bytebuffer-natives-real-jdk-direct-buffer-gaps.md`](s2-bytebuffer-natives-real-jdk-direct-buffer-gaps.md), reconfirmed unaffected by this fix with a fresh probe) вЂ” that doc stays open for its other residual items (2, 3, 4, 5, 6).
## 2026-07-12 Spring Boot loader `FileDataBlock` bulk-`ByteBuffer.put` AIOOBE FIXED (unblocked the entire `spring-boot-loader` module)

- FIXED/RETIRED (moved to `docs/internal/`): [`springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md`](../internal/fixed-suite-bugs/springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md) вЂ” `ByteBuffer.allocate(int)`/`wrap(...)`'s live dispatch site (`native-builtins/src/servlet.rs::bb_write_hb`) wrote a legacy indexed-slot fallback (`BB_MARK`=4, `BB_ARRAY`=0) unconditionally AFTER the correct by-name field writes, and those indices alias real `java.nio.Buffer`'s `address`(4)/`mark`(0) fields вЂ” clobbering `address` to `-1` on every heap `ByteBuffer`, so every bulk `put`/`get` via `ScopedMemoryAccess.copyMemory` threw `ArrayIndexOutOfBoundsException`. Fixed by gating the fallback behind the existing `s2_bb_synthetic_layout` real-vs-synthetic discriminator (the same mechanism a 2026-07-11 fix already used for the analogous `segment`/`BB_ORDER` collision) and adding the missing `address=16` seed. Also fixed a dead `ScopedMemoryAccess` native registration (stale JDK21-era descriptor). Verified via a standalone repro, two new unit tests, zero regressions in `cargo test -p cratonvm-native-builtins`, and a `loader/spring-boot-loader` module re-run (33 PASS/15 FAIL/2 EMPTY/1 HANG, up from the prior universal 9/9-class failure) вЂ” the residual FAILs are unrelated, separately-rooted bugs (multi-release jar version parsing, jar-signature verification, a file-handle-leak assertion, classpath URL enumeration) and the one HANG is a deliberately multi-GiB Zip64 fixture, not an infinite loop.

## 2026-07-12 ES `SegmentAllocator.allocate` interface-dispatch bug FIXED (blocked Elasticsearch native-access bootstrap suite-wide)

- FIXED/RETIRED (moved to `docs/internal/`): [`elasticsearch-suite/ES-FAIL-20260711-foreign-segmentallocator-dispatch-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-FAIL-20260711-foreign-segmentallocator-dispatch-FIXED.md) вЂ” `Arena.ofAuto()/ofConfined()/ofShared()/global()` allocate their return value under the literal interface name `java/lang/foreign/Arena`, but CratonVM's receiver-based dispatch retarget (`vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner`, the "C25" block) only re-points onto a receiver's own runtime class when that class is *concrete* (`!c.is_interface()`) вЂ” an invariant ordinary Java objects always satisfy but this synthetic Arena receiver does not. A real-JDK `SegmentAllocator` default method inherited by `Arena` (`allocate(MemoryLayout)`/`allocate(long)`, both real bytecode, used by Elasticsearch's own `JdkPosixCLibrary`/`NativeAccessHolder` native-access bootstrap) internally calls `this.allocate(byteSize, byteAlignment)`, an `invokeinterface` that resolved to the abstract `SegmentAllocator.allocate(long,long)` declaration instead of the already-registered `Arena.allocate(JJ)` native, throwing `AbstractMethodError: ... has no Code attribute` under both JIT on and off вЂ” blocking every ES test class that reaches `BootstrapForTesting`. Fixed by recomputing the receiver's actual runtime class directly from `args[0]` (independent of the interface-exclusion above) and checking its native registry too, generalising the existing "receiver's own-class native rescue" to interface-stamped synthetic receivers. Verified via `CRATONVM_DBG_NOCODE=1` (zero SegmentAllocator/Arena hits across `FastMathTests` and a 60-class slice, both JIT modes), a hand-compiled minimal probe (fails on baseline with the exact signature, passes on the fix), zero regressions on the full 52-class previously-passing set (an initial apparent 10-class flip under `-Parallel 4` was shared-host `/tmp` contention, not a code issue вЂ” confirmed identical via serial re-run and an unpatched-`origin/dev` A/B clone), and `cargo test -p cratonvm-vm --lib` (same 11 pre-existing, unrelated failures on both patched and baseline). Getting past this crash exposes ES's separate, already-tracked `EmbeddedImplClassLoader` nested-jar (jar-within-a-jar) Jackson classloading gap for most of the `others` category вЂ” see the doc's "Known residual" section вЂ” so `FastMathTests` itself doesn't yet reach a full 17/0 match with HotSpot, but the specific interface-dispatch bug this doc tracked is fully closed.

## 2026-07-12 ES `BinaryQuantizationTests#testQuantizeForQueryCosine` float divergence FIXED

- FIXED/RETIRED: [`elasticsearch-suite/ES-FAIL-20260711-binary-quantization-float-divergence-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-FAIL-20260711-binary-quantization-float-divergence-FIXED.md) вЂ” `org.elasticsearch.simdvec.ESVectorUtil.dotProduct`/`squareDistance` are intercepted by native Rust reimplementations that bypass the real Java/Lucene bytecode entirely (in both JIT and interpreter mode, explaining why the bug reproduced identically under `-Jit on`/`-Jit off`). The previous native did a naive sequential `sum += a[i]*b[i]`, but decompiling the actual bundled Lucene 10.4.0/ES 9.5.0-SNAPSHOT jars showed real HotSpot on this fixture runs Lucene's Panama-vectorized `dotProductBody`/`squareDistanceBody` (`--add-modules=jdk.incubator.vector` is on the harness's JVM args) вЂ” 4 independent 16-lane (512-bit) fma accumulators combined lane-wise then reduced via a strict sequential `reduceLanes(ADD)` fold (confirmed via OpenJDK's `FloatVector` fallback source, since these methods are called too few times per test to ever get JIT-intrinsified). Floating-point addition isn't associative, so the summation order has to match exactly. Fixed by mirroring that exact lane-grouped accumulation and reduction, with the lane count chosen via runtime AVX-512F/AVX2 CPU-feature detection (mirrors `VectorSpecies.ofPreferred`) rather than a hardware-specific constant. Also fixed a related, independently-discovered bug in the same native family: `java_math_round_f32` (used by the OSQ `calculateOSQLoss`/`quantizeVectorWithIntervals` natives) duplicated the naive, incorrect `floor(v+0.5)` `Math.round` formula instead of reusing the already-fixed bit-exact `lang_math::round_float`. Verified: `BinaryQuantizationTests` 8/8 PASS under CratonVM JIT-on and JIT-off, matching HotSpot (previously 1/8 FAIL under both CratonVM modes); a 26-class sweep of the broader binary-quantization/DiskBBQ test family showed no assertion-value regressions (the 18 failures in that sweep are all the pre-existing, unrelated `Suite timeout exceeded` suite-accounting bug documented in [`../internal/elasticsearch-restclient-builder-suite-timeout.md`](../internal/elasticsearch-restclient-builder-suite-timeout.md), which already calls out this exact class as historically affected by that separate issue).

## 2026-07-11 Spring Boot full-suite run (1975 classes) triaged; 5 bug clusters filed under `springboot/`

First full run of the real Spring Boot 4.1.0-SNAPSHOT test suite via the new
`apps/spring-boot-suite-runner` (see
[[project_spring_boot_suite_runner_20260711]]): 1975 classes, 1225 PASS / 508
FAIL / 150 HANG / 49 CRASH / 43 EMPTY. Triaged the 508 FAILs by log-signature
clustering; five distinct root causes characterized and filed under
[`springboot/`](springboot/README.md), covering ~146 classes directly (more
indirectly, since several of these produce the generic "Unstarted
application context" wrapper Spring shows at the test level):

- `OnClassCondition.addAll` NPE-cast-to-`String[]` (75 classes, 348 occurrences) вЂ” **FIXED/RETIRED 2026-07-13**, see the entry above.
- `DisposableBeanAdapter` "Invalid destruction signature" (34 classes) вЂ” **RESOLVED same day**: a direct probe against current dev found the destroy-method reflection path works fine; no distinct residual identified. See [`internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`](../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md).
- `sun.misc.Unsafe$MemoryAccessOption` NPE (26 classes, incl. indirectly via Netty's "failed to create a child event loop") вЂ” **FIXED/RETIRED same day**, independently found+fixed via a concurrent Keycloak/Infinispan investigation; see [`internal/fixed-suite-bugs/testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md`](../internal/fixed-suite-bugs/testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md) (canonical) and the Spring Boot corroborating occurrence alongside it.
- `HttpClient.Builder` dead real-JDK registration (13 classes) вЂ” **RESOLVED 2026-07-12**: every Java 17 fluent builder method is registered on the active one-field real-JDK path; see [`internal/fixed-suite-bugs/springboot-httpclient-builder-dead-registration-abstractmethoderror-FIXED.md`](../internal/fixed-suite-bugs/springboot-httpclient-builder-dead-registration-abstractmethoderror-FIXED.md).
- [`springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe.md`](springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe.md) вЂ” breaks the entire `spring-boot-loader` module (9/9 classes): bulk `ByteBuffer.put(ByteBuffer)`/`ScopedMemoryAccess.copyMemory` throws `ArrayIndexOutOfBoundsException` reading any zip/jar file through Spring Boot's own nested-jar loader.

Also fixed a general, VM-wide, deterministic crash found by this run (not
Spring-specific): [[reference_native_call_arg_pinning_unwrap_or_eager_eval_panic]]
вЂ” `Option::unwrap_or` evaluates its argument eagerly in Rust, panicking on
any native call with more than 4 arguments.

## 2026-07-11 Spring suite genuine-bug list reconfirmed (125 в†’ 96 open, 29 fixed)

Scoped rerun of exactly the 125-class list from the doc below on dev
`9948295e` (not a full 516-class HotSpot cross-reference). 29/125 now pass вЂ”
`core.test.tools.CompiledTests` was fixed by commit `2ed5f407`
(loader-defining-visibility fix in the real-JDK `loadClass` fast path); the
other 28 most likely benefited from the same fix (shared
`MockitoException`/`CompilationException`/CGLIB-proxy-`ABEND` symptoms), not
individually root-caused. 10 of the previously-documented 31 "newly broken /
HIB-CV-32" classes are among the 29 fixed. Four new failure clusters
characterized within the remaining 96 (not yet root-caused): an 11-class AOT
bean-registration TIMEOUT cluster (all hard-hang at the 120s ceiling), an
8-class Groovy scripting cluster (high per-method failure ratios), a broad
WebFlux reactive FAIL/EMPTY cluster, and 6 ABEND crashes with `found=0`
(crash before test discovery, distinct from the mid-run HIB-CV-32 crash
shape). See
[`CRATONVM-SPRING-GENUINE-BUGLIST-125.md`](CRATONVM-SPRING-GENUINE-BUGLIST-125.md)
for full detail.

## 2026-07-11 `HashMap` native-dispatch overhead вЂ” FIXED/RETIRED

Initial hypothesis (Integer autoboxing/allocation pressure) was wrong. cdb
stack-sampling (same technique used for this repo's `bintrees` GC/allocation-ceiling
profile) found the ratio is flat across sizes (not O(nВІ), unlike the sibling
String/Regex bug below) and NOT allocation-bound: only ~9% of sampled stacks were in
allocation/boxing paths vs ~90% for an allocation-bound workload. The real cost is
fixed per-native-call overhead in the JIT-to-native dispatch path вЂ” `HashMap.put/get`
and `Integer.hashCode()/valueOf()` are native Rust functions, and every call pays for
conservative JIT-frame root scanning (~29% of samples, the largest bucket),
RwLock-guarded class/field-layout lookups, and generic dispatch-machinery overhead.
Three targeted, behavior-preserving fixes shipped for the safely-addressable slice
(key hash/equals check-order in `native-collections/src/lib.rs`, a lock-free
receiver-corruption fast path in `vm/src/vm/vm_exec.rs`, a lock-free field-layout
cache in `gc/src/gen_heap.rs` mirroring an already-proven pattern elsewhere in this
codebase) вЂ” isolated 5-round re-measurement averaged ~206x post-fix, down from ~357x.
A follow-up addressed the root-publication and dispatch residual and reached 10.54x
the pinned CratonVM baseline; the
completed investigation is archived at
[`../internal/hashmap-native-dispatch-overhead.md`](../internal/hashmap-native-dispatch-overhead.md).

## 2026-07-11 TestParameterMap `replaceAll()` lock-bypass FIXED/RETIRED вЂ” real bug was an unwrapped `Map.Entry` escaping `Collections.unmodifiableMap(...).entrySet()`, not field visibility

The doc's own hypothesis (a plain-`boolean` `ParameterMap.locked` field-visibility
bug) was wrong. `checkLocked()` correctly saw `locked=true` on every call; the
actual gap was that CratonVM's synthetic `Collections.unmodifiableMap` wrapper's
`entrySet()` handed back the backing map's real, mutable `Map.Entry` objects
unwrapped, so `Map.replaceAll`'s default-method body (`for (Entry e :
entrySet()) e.setValue(v)`) silently mutated the "locked" map instead of
throwing `UnsupportedOperationException` вЂ” real JDK wraps each entry in
`Collections$UnmodifiableMap$UnmodifiableEntrySet$UnmodifiableEntry`. Also
found (and fixed by the same change) a previously-undocumented residual: the
successful mutation corrupted `tearDown()`'s subsequent value assertions.
Fixed by adding a dedicated `cratonvm/internal/UnmodifiableEntrySet` +
`UnmodifiableMapEntry` wrapper pair (`native-collections/src/lib.rs`,
`native-builtins/src/lib.rs`, `vm/src/vm/vm_init.rs`) so `setValue()` throws.
`TestParameterMap` 4/4 PASS; no regressions in a 31-test sweep of other
`Collections.unmodifiable*`/`entrySet()` consumers. See
[`parametermap-immutability-not-locked-FIXED.md`](../internal/fixed-suite-bugs/tomcat/parametermap-immutability-not-locked-FIXED.md).

## 2026-07-11 WildFly stale-ObjectRef sweep: systematic static-analysis pass finds+fixes ~37 more sites across 6 files; harness verification blocked by unrelated environment gap

Follow-up session 3 on [`wildfly-parallel-boot-stale-objectref-residual.md`](wildfly-parallel-boot-stale-objectref-residual.md),
acting on that doc's own recommendation after 2 sessions of one-off manual chasing found only 6-7 sites.
Built a line-oriented static-analysis scanner (masks comments/strings, tracks `ObjectRef`-like locals,
two-pass wrapper-hazard discovery for helper functions that internally call a GC-triggering `ctx.*`
method without being one themselves, interval-based "GC event between bind and use" detection) over
`native-builtins/src`, `native-io/src`, `classloading/src`. First pass found 1968 candidates; adding
wrapper-hazard detection found 3247. Manually triaged the ~2200 in files judged hot for WildFly
boot/reflection/classloading, fixing **~37 confirmed real instances** across
`jboss_module_loader.rs` (~15, including the singleton `build_local_module_loader` and
`native_loader_load_module`'s own `this`/`module` locals вЂ” directly on the WildFly bootstrap-module
path), `service_loader.rs` (8, including the `ServiceLoader.load(Class)` entry point itself),
`classloader.rs` (6), `classloader_real.rs` (4), `lang_reflect.rs` (2), `lang_class.rs` (2, including
another `Constructor.newInstance`-family site in the serialization-constructor branch, and the
never-before-fixed `build_serialized_lambda`). All fixed with the same established
`pin_native_root`/`read_native_pin`/`unpin_native_roots` idiom. Consolidated 9 identical
exception-construction call sites into one new shared helper (`alloc_single_message_exception`) rather
than repeating the pin dance inline.

- Verified: clean `cargo check`, full `cargo test -p cratonvm-native-builtins --lib` (2961/2965; the 4
  failures confirmed via `git stash` to be pre-existing/unrelated), and byte-for-byte non-regression
  against the last known-good frozen binary on the one live repro attempted.
- **Not** verified: a live clean WildFly boot improvement вЂ” the shared Azure host's provisioned test
  distribution is missing its `modules/` directory entirely (`apps/wildfly/build/target/`, the Maven
  module that provisions it, doesn't exist on this checkout), a pre-existing environment gap unrelated to
  this session's code, confirmed by reproducing the identical failure against the last verified-good
  binary from a prior session. See the residual doc's "Follow-up session 3" section for the full story
  and what whoever next has a working harness should re-run.
- Also **implemented** (same session, after initial scoping): a debug-build assertion,
  `CRATONVM_DBG_STALE_OBJREF=1`, that catches this whole bug class deterministically at runtime for the
  `Generational` (default) GC backend вЂ” a stale native local read within one GC cycle of evacuation now
  hard-panics instead of silently corrupting. Reuses the GC's own existing forwarding-pointer header field
  (no new tombstone format needed) behind a one-cycle quarantine delay on reclaiming evacuated memory; see
  [`../internal/wildfly-stale-objectref-debug-assertion-scoping.md`](../internal/wildfly-stale-objectref-debug-assertion-scoping.md)
  for the mechanism and its explicit scope boundaries (G1/ZGC not covered).
- Remaining untriaged: `lang_class.rs`'s other 114 candidates, `lang_invoke.rs`, `servlet.rs`,
  `jboss_msc.rs`, the `wildfly_*.rs` files, `spring_startup_bootstrap.rs`, and three giant
  "Phase N native registration" files (`lib.rs`/`phases_late.rs`/`phases_early.rs`, ~1550 combined
  candidates) вЂ” a spot-check of `lib.rs` alone found another real bug
  (`spring_xml_set_factory_bool`'s caller), not yet fixed.

## 2026-07-11 JIT BCE: missing AIOOBE + silent OOB heap write on multi-array loops (OPEN, Severity: HIGH)

Found while validating the GPU offload deopt path (the bug itself is in the CPU JIT, not the
GPU stack). Bounds-check elimination on a counted loop indexing multiple arrays by the same
induction variable (e.g. `for (i=0;i<a.length;i++) out[i]=a[i]+b[i];` with `out.length <
a.length`) elides the bounds check on the shorter array using the longer array's length as the
loop bound вЂ” with the JIT/OSR on, the method returns normally with no
`ArrayIndexOutOfBoundsException` and silently writes past the end of `out` into whatever object
follows it on the heap. HotSpot JDK 25 and CratonVM `--nojit` both throw the required AIOOBE.
See [`jit-bce-multi-array-oob-store-20260711.md`](jit-bce-multi-array-oob-store-20260711.md) for
the minimal repro (`test_classes/gpu/BoundsDeopt2.java`) and the suspected BCE mechanism.

## 2026-07-11 GPU offload: first real-hardware validation passed; 7 follow-ups filed (OPEN, none blocking)

The GPU offload validation follow-ups from the RTX 2060 hardware pass are
complete as code work and have moved to the
[`docs/internal` record](../internal/gpu-offload-followups-20260711.md).
Operational GPU-runner enrollment is tracked with the CI infrastructure rather
than as a known runtime issue.

## 2026-07-11 Regex `find()`+`group()` quadratic slowdown FIXED вЂ” two wrong turns (dead-code Matcher bridge, dead-code substring native) before finding the real bug in the live one

A user-reported benchmark (`StringBuilder` append loop + `Pattern.compile().matcher()`
+ `while (m.find()) { m.group(1); }`) showed a CratonVM-vs-JDK slowdown ratio that
*grew* with input size (18.8Г—/94.4Г—/238.8Г— at 1K/5K/10K entries) вЂ” the tell for an
algorithmic-complexity bug. First diagnosis blamed `Matcher`'s native bridge
(`matcher_read_input()`) вЂ” a real O(nВІ) bug, but instrumentation proved that
bridge is unconditionally dropped in real-JDK mode
(`registry.rs`'s `drop_real_layout_synthetic`, same "synthetic bridge corrupts
real-layout objects" family as
[`stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md`](../internal/fixed-suite-bugs/stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md))
вЂ” the fix, while correct, was dead code. That led to isolating the actual bug to
plain `String.substring()` (zero regex involved) вЂ” but the FIRST substring native
found and instrumented (`native_string_substring`,
`native-builtins/src/lang_string.rs`) was ALSO dead code, this time because its
registration lives inside `register_synthetic_overrides`,
`#[cfg(feature = "synthetic-jdk")]`-gated and not compiled into the real-JDK
build at all. The actual live registration вЂ” a separate inline closure in
`register_essential_natives` вЂ” had the identical "decode the entire parent
string, every call" bug independently. **FIXED**: see
[`../internal/fixed-suite-bugs/substring-large-parent-quadratic-allocation-FIXED.md`](../internal/fixed-suite-bugs/substring-large-parent-quadratic-allocation-FIXED.md)
for the full (long) story and the fix. `SubstringOnly` 1283msв†’29ms at n=10,000;
the original combined benchmark 4775ms (never finished at n=50,000)в†’2295ms
(completes) вЂ” checksums identical to JDK throughout, zero test regressions.

## 2026-07-11 WildFly Surefire-fork boot-crash 2nd follow-up: decompiled DeferredExtensionContext, found+fixed 2 MORE GC-staleness sites (live-caught in the act); residual is a long-tail bug class, not a small fixed set

Follow-up to the entry directly below's higher-priority lead. Decompiled
`org.jboss.as.controller.parsing.DeferredExtensionContext` (`wildfly-controller-31.0.3.Final.jar`) and
confirmed it genuinely loads extensions concurrently: one `Callable` per extension submitted to a
`bootExecutor` `ExecutorService`, each independently calling
`moduleLoader.loadModule(name).loadService(Extension.class)`, then blocking on `Future.get()` per
extension (surfacing `ExecutionException` as the observed `IllegalStateException`).

- FIXED: `native_module_load_service`/`native_module_load_service_from_caller_module_loader`
  (`native-builtins/src/jboss_module_loader.rs`) held `service_type`/`service` (the `Class` mirror for
  `Extension.class`) across `native_module_get_class_loader` вЂ” which lazily allocates a new
  `ModuleClassLoader` the first time it's asked for a given module, i.e. essentially every
  extension-loading call during boot вЂ” before using it again, unpinned.
- FIXED: `native_sl_iterator` (`native-builtins/src/service_loader.rs`) held the reflective `Constructor`
  across an intervening `AccessibleObject.setAccessible` invoke *and* a `new_ref_array` allocation before
  its second use in `Constructor.newInstance`. Caught directly in the act via a live
  `CRATONVM_DIAG_SERVICELOADER=1` capture: the module-scoped provider lookup correctly found
  `org.wildfly.extension.beanvalidation.BeanValidationExtension` (`providers=1`), but a *later* attempt
  to instantiate that exact class failed with `"Constructor.newInstance: no declaring class"` вЂ” the same
  symptom this whole investigation started with, now proven to recur at an entirely different call site
  than the one originally fixed.
- Both fixes verified as real, non-regressing improvements (8-retry targeted sample: 4/8 clear, 3/8
  original NPE, 1/8 still a related `WFLYCTL0153` failure) but **did not fully eliminate the residual**.
- OPEN, re-characterized: [`wildfly-parallel-boot-stale-objectref-residual.md`](wildfly-parallel-boot-stale-objectref-residual.md) вЂ”
  after 6-7 total sites of this exact "Family 1" pattern found and fixed across 3 sessions (this one,
  the one below, and the original fix), with the residual still not fully closed, this is now understood
  as a **long-tail bug class** rather than a small enumerable set of sites. The doc recommends a
  systematic static-analysis sweep (scan for `ObjectRef`/`Value` locals bound before an allocating `ctx.*`
  call and read again without an intervening pin) over continued one-off manual chasing.

## 2026-07-11 WildFly Surefire-fork boot-crash follow-up: 2 more GC-staleness sites FIXED (6/10 в†’ 9/10 sample); residual narrowed to a concurrent extension-loading race + the known STW JIT-takeover stall

Follow-up to the entry directly below. Investigating the residual's two reported symptoms
(`ProcessorInfo.readCPUMask()` `NoSuchMethodError`, `ParallelBootOperationStepHandler` NPE) found the
identical unpinned-`ObjectRef`-across-`ensure_class_initialized`+`alloc_object` pattern in two more
functions, both backing *every* `InputStreamReader`/`OutputStreamWriter` construction VM-wide:

- FIXED: `native-io/src/stream_decoder.rs::alloc_stream_decoder` and
  `native-io/src/stream_encoder.rs::alloc_stream_encoder` held their `InputStream`/`OutputStream`
  parameter across the same two GC-risking calls (`ensure_class_initialized("sun/nio/cs/StreamDecoder"
  /StreamEncoder")`, `alloc_object`) before storing it into the new `StreamDecoder`/`StreamEncoder`'s
  field вЂ” same "Family 1" pattern as the sibling `lang_class.rs` fix. Verified: re-running the identical
  10-class sample against a binary with all three fixes raised the "clears the original crash" rate from
  6/10 to **9/10**.
- OPEN, better characterized (updated): [`wildfly-parallel-boot-stale-objectref-residual.md`](wildfly-parallel-boot-stale-objectref-residual.md) вЂ”
  the one remaining class in the sample does **not** deterministically hit the original crash: 3 runs
  against the identical fixed binary gave 3 *different* outcomes, including a *new* signature
  (`WFLYCTL0153: No META-INF/services/org.jboss.as.controller.Extension found`) for a *different*
  specific extension each time. This points at a genuinely concurrent race in WildFly's own
  `DeferredExtensionContext`/`FutureTask`-based extension loading, not another single fixed
  unprotected-`ObjectRef` site. Separately reconfirmed (via a live-attach attempt, though it missed the
  exact stall window) that the STW cross-thread JIT-takeover stall documented in
  `wildfly-gc-barrier-boot-hang-and-harness-fixes.md` (main-thread instance fixed; the
  EnhancedQueueExecutor-worker-parked-in-futex instance explicitly left OPEN as high-regression-risk
  deep GC-barrier work) still reproduces on current dev вЂ” several "boots further, still fails" classes
  show the identical `rounds=64 ... taken=0` signature.

## 2026-07-11 WildFly Surefire-fork boot-crash (96% of suite failures) FIXED вЂ” reflection-object GC-staleness; residual stale-`ObjectRef` sites found elsewhere in boot

Root-caused and fixed the dominant blocker for the WildFly suite under CratonVM: the managed server
spawned by Arquillian from within a CratonVM-run Surefire fork exited with code 1 before writing a
single line to `server.log`, in 583/605 (96%) of `testsuite/integration/basic` failures in round 6.

- FIXED/RETIRED: [`wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md`](../internal/fixed-suite-bugs/wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md) вЂ”
  `create_constructor_object`/`create_method_object`/`create_field_object`
  (`native-builtins/src/lang_class.rs`) held their freshly-`alloc_object`'d instance (and its
  `class_mirror`/`parameterTypes`/etc. locals) as unpinned `ObjectRef`s across several subsequent
  GC-triggering classloading calls, in violation of the documented `pin_native_root` contract. A moving
  GC landing in that window (reliably triggered by WildFly's `ServiceLoader`-based extension bootstrap,
  ~500-750 module jars) corrupted the returned reflection object, surfacing as
  `Constructor.newInstance: no declaring class` for several `Extension` SPI providers (Elytron, IO,
  SecurityManager, clustering) вЂ” which silently dropped those extensions from the registry, cascading
  into `AbstractControllerService`'s `this.controller is null` NPE crashing boot before any logging
  subsystem could open `server.log`. Isolated repros of the exact same captured launch command never
  reproduced this because a minimal, non-Surefire-forked process doesn't generate enough concurrent
  classloading pressure to reliably land a GC in the danger window. Fixed by pinning + re-reading
  forwarded references in all three constructors, mirroring the pattern `build_mirror_array_comp`
  already used internally. Verified against the real harness (not an isolated repro): the specific
  `ServiceLoader` corruption warning is gone in every subsequent run, and 6/10 sampled previously-crashing
  classes now boot far past the original crash point (60-70s of real subsystem processing instead of an
  instant 8-13s crash).
- OPEN (new, split off вЂ” same general bug class, different call sites, not fixed by the above):
  [`wildfly-parallel-boot-stale-objectref-residual.md`](wildfly-parallel-boot-stale-objectref-residual.md) вЂ”
  4/10 sampled classes still hit the identical `this.controller is null` crash (deterministically for at
  least one class across 3 retries), and classes that now boot further sometimes hit a *different* pair
  of failures bearing the same "stale `ObjectRef` resolves to a reused all-zero-header slot" fingerprint:
  a `NoSuchMethodError: java/lang/Object.read([CII)I` in `ProcessorInfo.readCPUMask()`, and a
  `NullPointerException` on `ModelValue.has` inside `ParallelBootOperationStepHandler`'s
  `EnhancedQueueExecutor` worker threads (a genuinely multi-threaded context). Needs its own
  investigation before a full-suite re-run can give an accurate post-fix failure count.

## 2026-07-11 Uncaught-exception fatal-error misattribution FIXED (`java/lang/Thread`/`CommonToken` reported instead of the real Throwable); 2 real bugs unmasked

Investigated the confirmed-but-unexplained pattern already flagged in
`CRATONVM-SPRING-GENUINE-BUGLIST-125.md` (many `ABEND rc=1` classes reporting
`Exception in thread "main" java/lang/Thread`/`org/antlr/v4/runtime/CommonToken`
вЂ” neither a `Throwable` subclass) via a fresh Hibernate ORM repro
(`FunctionTests`, `ASTParserLoadingTest`, `DefaultCatalogAndSchemaTest`,
`OneToOneJoinColumnsEmbeddedIdTest`).

- FIXED/RETIRED: [`uncaught-exception-misattribution-native-pending-return-FIXED.md`](../internal/fixed-suite-bugs/uncaught-exception-misattribution-native-pending-return-FIXED.md)
  вЂ” `JvmThread::native_pending_return` (a native call's return-value GC root,
  cleared once pushed to the caller's operand stack) was being consulted
  **unconditionally** at two exception-unwind sites whenever it happened to
  hold a leftover value from an unrelated earlier native call, silently
  replacing the real, correctly-thrown exception with whatever stale object
  (e.g. an ANTLR `CommonToken`, or a `Thread` mirror) was sitting there.
  Fixed by only falling back to it when the real exception object has
  actually gone stale (GC-relocated during the failing call), mirroring the
  staleness check `safe_native_call` already used elsewhere. Verified across
  all 4 repro classes: zero crashes post-fix (previously 100%), one class
  (`OneToOneJoinColumnsEmbeddedIdTest`) now runs to full completion.
- FIXED/RETIRED (same day, follow-up): [`onetoone-embeddedid-propertyaccessexception-FIXED.md`](../internal/fixed-suite-bugs/onetoone-embeddedid-propertyaccessexception-FIXED.md)
  вЂ” the `org.hibernate.PropertyAccessException` this fix unmasked in
  `OneToOneJoinColumnsEmbeddedIdTest` (3/6 tests) is fixed too:
  `Field.set`/`Method.invoke`/`Constructor.newInstance`'s reflective
  argument-coercion check resolved the expected reference type via a
  global, loader-chain-first name search, which can resolve to the WRONG
  same-named class when a class is legitimately loaded under two different
  classloaders (confirmed: Hibernate's bytecode enhancement reloads
  `@EmbeddedId` classes under a private ByteBuddy-style loader, distinct
  from the original `Application`-loader `ClassId`) вЂ” rejecting a
  perfectly-typed value as a mismatch. Fixed with a new
  `class_id_by_name_near` resolution that prefers the SAME loader as the
  declaring `Field`/`Method`/`Constructor`. `OneToOneJoinColumnsEmbeddedIdTest`:
  `ok=3 failed=3` в†’ `ok=6 failed=0`. Verified no regressions via a
  115-class sample of `passed.txt` cross-checked against the pre-fix
  baseline for every non-PASS result.
- OPEN (new, unmasked by the fix, host-load-limited): [`functests-astparser-defaultcatalog-post-fix-slow-untriaged.md`](functests-astparser-defaultcatalog-post-fix-slow-untriaged.md)
  вЂ” `FunctionTests`/`ASTParserLoadingTest`/`DefaultCatalogAndSchemaTest` no
  longer crash and now run far more of the real suite, but didn't reach a
  clean `@@RESULT` within the time available on a heavily contended shared
  host (concurrent ES/Tomcat suite runs from other sessions). `FunctionTests`'s
  masked exception was confirmed (before the fix, via live instrumentation)
  to be a genuine `NullPointerException` inside the heavily-parameterized
  `testDurationArithmeticWithParameters`; needs an idle-host rerun with
  `-Dcraton.trace=1` to pin down further.


## 2026-07-10/11 ES `RandomBinaryDocValuesRangeQueryTests` hang cluster: 4/4 FIXED (compact-field getfield bug fixed upstream; InetAddress CONTAINS false negative fixed 2026-07-11)

- FIXED/RETIRED: [`long-random-binary-doc-values-range-query-tests-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/long-random-binary-doc-values-range-query-tests-FIXED.md), [`integer-random-binary-doc-values-range-query-tests-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/integer-random-binary-doc-values-range-query-tests-FIXED.md), [`double-random-binary-doc-values-range-query-tests-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/double-random-binary-doc-values-range-query-tests-FIXED.md) вЂ” all three classes' original 600s suite-timeout HANG (collected 2026-07-08, `LRUQueryCache`'s internal `ReentrantReadWriteLock`/`ReentrantLock` write-lock contention) had turned into a 100% deterministic JIT SIGSEGV on a binary built strictly after that collection: `ReentrantLock.unlock()`'s single getfield (`this.sync`) was compiled as a 32-bit sign-extending `movsxd` load instead of a 64-bit `mov`, corrupting the loaded receiver before dispatching `sync.release(1)`. Root cause: `compact_field_slot(...).unwrap_or((0, false))` in three `field_resolver` closures (`vm/src/runtime/interpreter.rs`) silently fabricated "offset 0, not a reference" whenever a field's declaring class had no registered compact layout, and `jit/src/lib.rs`'s scan step trusted that fabrication unconditionally, steering the getfield/putfield inline codegen to treat a genuine reference field as a primitive. Independently root-caused and fixed by a concurrent session via a third, unrelated symptom (WildFly Host Controller invoke-IC SIGSEGV) вЂ” see this file's own `7f96c26c`/`be710234`/`93b33576` entries. Verified 2026-07-10 on a clean checkout of dev tip `e768916a` (no local changes needed): all three classes pass cleanly (`OK (6 tests)`) under fully default JIT settings.
- FIXED/RETIRED: [`elasticsearch-suite/ES-HANG-20260709-server-org-elasticsearch-lucene-queries-inetaddressrandombinarydocvaluesrangequerytests-51a9c7ea93-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-HANG-20260709-server-org-elasticsearch-lucene-queries-inetaddressrandombinarydocvaluesrangequerytests-51a9c7ea93-FIXED.md) вЂ” the same SIGSEGV/hang mechanism was gone here too, but the class ran to completion and hit a **different, genuine correctness bug**: a `CONTAINS`-query false negative for a query range spanning an IPv4 min and IPv6 max against a stored box, whose max always printed with a trailing `/0.0.0.0`. Root-caused 2026-07-11 to two stacked defects in `native-builtins/src/net_phase_e.rs`'s `InetAddress` mirror machinery: (1) its process-global side table was never registered as a GC root (same bug class as `BUG-U`'s stale Locale), so a moving GC relocating a live mirror left it keyed on a vacated slot; (2) the fallback reader never checked the real `Inet6Address` holder6 field, so any miss (or any address built via the un-overridden two-arg `getByAddress(String,byte[])`) reported `"0.0.0.0"` instead of the real value. Fixed by adding `gc_scan_inet_addr_roots`/`gc_update_inet_addr_refs` (mirroring the existing Locale root-scan pattern) and fixing the holder6 fallback. Verified: 6/7 pre-fix runs reproduced the exact signature, 0/9 post-fix runs did. Two unrelated, rare, pre-existing failures surfaced during verification (a `java/util/Set` GC-staleness NPE; a `ClassCastException` since confirmed as a third real-world corroboration of the already-tracked monitor-vs-evacuation race, see `docs/internal/gc-audit-2026-07-10-open-findings.md` finding 1(b)) вЂ” neither blocks this retirement.

## 2026-07-10 AccessLogValve/RewriteValve doc RETIRED (5/6 causes fixed; 6th is the already-tracked register-invisible-JIT-root family, not a new bug)

- RETIRED: [`tomcat/accesslogvalve-rewritevalve-connection-failures-RESOLVED.md`](../internal/fixed-suite-bugs/tomcat/accesslogvalve-rewritevalve-connection-failures-RESOLVED.md) (moved from `known-issues/tomcat-08-07/`) вЂ” five of six root causes found across this investigation (`URL.openConnection()` CCE, `ByteBuffer.address`, `StringReader.read()`, the cross-cutting `SocketWrapperBase.lock` NPE, and a JIT `ConcurrentLinkedQueue` allocate-then-CAS miscompile) are FIXED and landed on `dev`. The sixth вЂ” a SIGSEGV around `TestAccessLogValve` test #8 вЂ” is a confirmed, byte-for-byte register-signature match with the already-tracked, currently-OPEN "register-invisible JIT root" bug family (real fix needs precise JIT oop maps / shadow stack, deep infrastructure work, deliberately not attempted). Catalogued as another occurrence in [`tomcat/swallowabortedupploads-unexpected-socketexception-RESOLVED.md`](../internal/fixed-suite-bugs/tomcat/swallowabortedupploads-unexpected-socketexception-RESOLVED.md) (also since retired вЂ” see below), the tracking doc for this family вЂ” don't reopen either retired doc for a repeat of this signature, catalogue it as a new occurrence somewhere fresh instead.
- **Correction while retiring:** the retired doc's sixth-cause section had cited `hib-global-temptable-nondeterministic-sigsegv-20260710.md` as a corroborating occurrence of this family. That's stale вЂ” see the entry above (2026-07-10 Hibernate remote rerun SIGSEGV cluster RESOLVED): that cluster was a different, unrelated, already-fixed bug. Corrected in both the retired doc and the swallow-uploads tracking doc.

## 2026-07-11 `TestSwallowAbortedUploads` doc RETIRED вЂ” full class passes clean

- RETIRED: [`tomcat/swallowabortedupploads-unexpected-socketexception-RESOLVED.md`](../internal/fixed-suite-bugs/tomcat/swallowabortedupploads-unexpected-socketexception-RESOLVED.md) (moved from `known-issues/tomcat-08-07/`) вЂ” `org.apache.catalina.core.TestSwallowAbortedUploads` now passes all 10 tests clean (`OK (10 tests)`, verified 4Г—). This doc's history spans 8 distinct, genuine defects across ~10 sessions (the original socket-close overcorrection, a `ScheduledThreadPoolExecutor` boot blocker, a `ByteBuffer` connector `AbstractMethodError`, the cross-cutting `SocketWrapperBase.lock`/`LinkedBlockingDeque` synthetic-layout NPE, a `String(char[])` interpreter-throughput gap that tripped a 3s connector timeout, the register-invisible-JIT-root `SB-CRASH-04` SIGSEGV, and finally the `sc_close` swallow-vs-abort gap itself plus a `java.net.Socket` write-path exception-classification bug) вЂ” read the retired doc's own chronology for the full arc before assuming a superficially-similar future symptom is one of these already-closed causes.

## 2026-07-11 `testNonBlockingReadIgnoreIsReady`: fixed-length HTTP streaming FIXED

**Resolved (2026-07-11):** The Acceptor/Poller finding below was disproved
by a minimal fixed-length-streaming `HttpURLConnection` reproducer. The
legacy bridge buffers its body locally and only opens/sends the request at
response retrieval; direct `Socket` clients are accepted promptly. The active
record is archived at [`httpurlconnection-fixed-length-streaming-deferred-FIXED.md`](../internal/fixed-suite-bugs/tomcat/httpurlconnection-fixed-length-streaming-deferred-FIXED.md).

The implementation now sends real-carrier fixed-length HTTP request heads and
body writes immediately; both `testNonBlockingReadIgnoreIsReady` and
`testNonBlockingRead` pass. The remaining text in this section is retained as
historical root-cause evidence.

Re-investigated
[`tomcat/nonblockingreadignoreisready-async-error-response-completion-gap.md`](tomcat/nonblockingreadignoreisready-async-error-response-completion-gap.md).
Its "container commits an implicit 200 response that never flushes" theory
does not hold up: verified directly against real HotSpot that the test
actually passes via a genuine client-side `IOException` thrown mid-upload,
not `rc=200`. Root-caused via a correlated Rust+Java timeline instead: the
client finishes its entire ~2s, `Thread.sleep`-paced write loop and closes
its socket *before* CratonVM's Tomcat connector ever performs its first
read вЂ” so the timing race HotSpot depends on (server reacting to a
misbehaving `ReadListener` before the client's next write) never happens.
Confirmed this is not a JIT-warm-up artifact (reproduces identically even
after a same-JVM warm-up test) and not the earlier-hypothesized
`native-io` socket-close/drain-timeout issue (tested directly, zero
effect вЂ” the peer had already sent EOF long before close() ran).

- SUPERSEDED/RETIRED: [`nio-poller-acceptor-thread-scheduling-latency-SUPERSEDED.md`](../internal/fixed-suite-bugs/tomcat/nio-poller-acceptor-thread-scheduling-latency-SUPERSEDED.md)
  вЂ” the originally-claimed mechanism (NioEndpoint `Acceptor` thread appears to
  make no progress for ~2 seconds while `Poller` is independently parked in
  blocking `select()`/`WSAPoll`, then both make rapid progress together) does
  **not** hold up: isolated Rust unit tests confirmed the low-level
  `wakeup()`/`select()`/registration primitives are each individually fast and
  correct, the Acceptor entered native `accept()` immediately, and a direct
  `Socket` client was accepted promptly under the same NIO/Poller shape. The
  apparent stall was entirely client-side (see the correction above). Moved to
  `docs/internal/` вЂ” its primary claim is refuted and the real, still-open
  issue was resolved in `httpurlconnection-fixed-length-streaming-deferred-FIXED.md`
  (linked above), which is currently owned by another concurrent session.

## 2026-07-10 ES suite-wide `Build$CurrentHolder` manifest-null FIXED (VM-core `Unsafe` bootstrap bug); new pre-existing Jackson residual filed

- FIXED/RETIRED: [`ES-FAIL-FAMILY-20260710-build-currentholder-manifest-null-FIXED.md`](../internal/fixed-suite-bugs/ES-FAIL-FAMILY-20260710-build-currentholder-manifest-null-FIXED.md) вЂ” every affected class (265 rows in the original partial run) crashed at bootstrap with `ExceptionInInitializerError`/`NullPointerException` from `Build$CurrentHolder.findCurrent()`, `manifest` being null. Root cause was NOT ES-specific: `jdk/internal/misc/Unsafe.<clinit>` computes its 9 `ARRAY_*_BASE_OFFSET`/9 `ARRAY_*_INDEX_SCALE` static constants by calling natives (`arrayBaseOffset0`/`arrayIndexScale0`) that aren't registered yet this early in real-JDK-mode bootstrap вЂ” the calls silently return 0 instead of throwing, so `ARRAY_BYTE_BASE_OFFSET` stays permanently latched at 0. `java.util.zip.ZipUtils.get32/get16` (used by `ZipInputStream.getNextEntry()`'s LOC-header parser, in turn used by `JarInputStream.getManifest()`) then reads 16 bytes short of where the header actually starts, the signature check silently fails, and `getManifest()` returns null with zero exceptions anywhere in the chain вЂ” reproduces for any jar, not just ES's. Fixed with a `jdk/internal/misc/Unsafe` post-clinit success-path backfill (`vm/src/vm/vm_util.rs`), mirroring the existing `UnsafeConstants` fixup for the identical bug shape.
- OPEN (new, pre-existing, unmasked by the fix above): [`elasticsearch-suite/ES-xcontent-jackson-streamreadconstraints-loader-blind-invokestatic.md`](elasticsearch-suite/ES-xcontent-jackson-streamreadconstraints-loader-blind-invokestatic.md) вЂ” the representative class (`EcsJsonUtilsTests`) now reaches real test execution and hits a separate, already-known `invokestatic` loader-blind-resolution bug (`NoSuchMethodError: StreamReadConstraints$Builder.maxNameLength`) instead. The 265-row family needs a fresh full-suite run against the fixed binary to re-triage what (if anything) remains per class вЂ” out of scope for this fix.

## 2026-07-10 Hibernate remote rerun SIGSEGV cluster RESOLVED (== guarded-inline-getfield JIT regression)

- RESOLVED/RETIRED: [hib-global-temptable-nondeterministic-sigsegv-20260710-RESOLVED.md](../internal/fixed-suite-bugs/hib-global-temptable-nondeterministic-sigsegv-20260710-RESOLVED.md) вЂ” the "20/33 classes SIGSEGV" Hibernate cluster is **not** a global-temp-table race; it is the **guarded-inline JIT `getfield` regression** (introduced by `07dfa5e0`, per `93b33576`s bisection), **already fixed on `dev` default builds by `93b33576`** (flip `guarded_inline_getfield_enabled()` to opt-in). Verified 2026-07-10 by an A/B on a dev-HEAD binary (fast path OFF = 8/8 clean PASS of `type.temporal.InstantTests`; `CRATONVM_JIT_GUARDED_GETFIELD=1` = 8/8 SIGSEGV) plus a fresh gdb backtrace matching `93b33576`s `0x40`-as-pointer signature. The reported rc=124 HANGs are the separate pre-existing environmental hang; the original global-temp-table-race hypothesis is refuted.
## 2026-07-10 ES TaskInfoTests JIT SIGSEGV FIXED вЂ” compact-ref-field/legacy-layout String dual-dispatch gap

- FIXED: while reverifying the (already-retired) `ES-CRASH-20260709-server-org-elasticsearch-tasks-taskinfotests-ba6bf27e77.md`/`ES-CRASH-FAMILY-20260709-currentdev-fail-probe-rc139.md` `findNative`-crash docs on current `dev` вЂ” that crash family is confirmed gone, but `TaskInfoTests.testFromXContent` hit a *different*, previously-masked JIT SIGSEGV. `StringFieldLayout::new` (`jit/src/lib.rs`) computed each `java/lang/String` field's byte offset as `HEADER_SIZE + idx * SLOT_SIZE`, assuming every field occupies a uniform 16-byte `Value` cell вЂ” but under `CRATONVM_COMPACT_REF_FIELDS` (default on) `String.value` is a bare 8-byte pointer, so every following field's computed offset was 8 bytes too high. The JIT's inlined `indexOf`/`equals`/`hashCode`/`compareTo` intrinsics then read a neighboring field's tag/payload word as if it were a pointer and dereferenced it вЂ” a deterministic wild-pointer SIGSEGV, not a race. A second, related instance surfaced once the first was fixed: compact-ref-field and *legacy*-laid-out `String` instances can coexist at runtime for the same class (an allocation whose field count didn't match the registered `CompactLayout` at alloc time keeps the old uniform-slot layout), so a single static offset can never be correct for both вЂ” fixed with per-object `GC_FLAG_COMPACT` header-bit dispatch added to the same String intrinsics in `jit/src/x64.rs` (`emit_load_string_value_ptr`/`emit_load_string_i32_field`), choosing the right offset per receiver instead of per class.
- Verified via a 52-class ES `passed`-category A/B regression (this fix vs. pure `origin/dev` at the same base commit): zero status regressions, plus 3 bonus fixes вЂ” `AnalysisModuleTests`, `StableAnalysisPluginsNoSettingsTests`, `StableAnalysisPluginsWithSettingsTests` all went `CRASH`в†’`FAIL` (same underlying String-layout bug, different call sites).
- The `mainLock`/`ctl` NPE-and-hang family this investigation also turned up in `Executors.newFixedThreadPool`/`newCachedThreadPool`/`newSingleThreadExecutor` was independently and more thoroughly fixed by concurrent sessions in the same window (`306cd352`, `825fbd9b` et al. вЂ” real `ThreadPoolExecutor`/`Thread` construction via `invoke_special`); no action needed here.
- Merged as `afa4a6fd`.

## 2026-07-10 TestJspDocumentParser SAXParseException RETIRED вЂ” real bug was a FileInputStream.close() field-clobber, not a Xerces/SAX defect

- FIXED/RETIRED: [`jspdocumentparser-saxparse-malformed-markup-FIXED.md`](../internal/fixed-suite-bugs/tomcat/jspdocumentparser-saxparse-malformed-markup-FIXED.md) вЂ” the original diagnosis (Xerces rejecting well-formed markup HotSpot accepts) was wrong; a direct SAX parse of the real `bug54801a/b.jspx`/`valid.jspx` bytes always succeeded on CratonVM. The actual bug: `native_fis_close` (`native-io/src/lib.rs`) unconditionally wrote `Value::Int(-1)` into `FileInputStream` instance slot 0 вЂ” the real `fd: Ljava/io/FileDescriptor;` reference field in real-JDK mode вЂ” clobbering it to `null` whenever this native got dispatched via `ctx.invoke_virtual` (as `sun.nio.cs.StreamDecoder`'s native `close()` does when closing its wrapped stream) before any ordinary bytecode call had already cached real bytecode as the winning resolution for `(FileInputStream, close, ()V)`. This broke Jasper's `JDTCompiler` reading its own generated `.java` source (`FileInputStream`+`InputStreamReader`+`BufferedReader` in one try-with-resources), so `testBug54801`/`testBug54821`/six `testDocument_*` cases 500'd on JSP compile, and `testSchemaValidation` then XML-parsed Tomcat's resulting HTML error page (correctly rejecting `<!doctype html>` as malformed XML) вЂ” that secondary failure is what gave this bug its misleading name. Fix: only mirror the closed-marker into slot 0 when no real `FileDescriptor` object exists (matching the guard `fis_set_fd` already used). `TestJspDocumentParser` 11 failures в†’ 22/22 PASS. Fixed+merged via `fix/jspdocumentparser-sax-20260710-001`.

## 2026-07-10 ES libs/tdigest SortingDigestTests both correctness clusters RETIRED; testMonotonicity perf issue split out

- FIXED/RETIRED: [`ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness-FIXED.md) -- both clusters fixed. `-Jit off` (`NoSuchMethodError: java/lang/Object.get(I)D` + log4j `ReusableParameterizedMessage` CCEs): `allocate_lambda_proxy` (`vm/src/runtime/invokedynamic.rs`) popped a lambda's captured values into an untracked Rust `Vec` before allocating the proxy object; a GC triggered on the young-gen-exhaustion retry path could relocate a captured object while unrooted, writing a stale `ObjectRef` into the new proxy's field -- every later dispatch through that proxy resolved `ClassId(0)`/`java.lang.Object` instead of the real receiver class. `-Jit on` (garbage-index `ArrayIndexOutOfBoundsException`, index always decoding as a plausible heap pointer, never a plausible array index): On-Stack Replacement corruption specific to `java/util/DualPivotQuicksort.sort`'s two self-recursive overloads (reached via `SortingDigest.compress()` -> `values.sort()`), bisected via `CRATONVM_JIT_OSR=0` (fixes it) and `CRATONVM_DBG_OSR=1` (shows these are the ONLY methods ever OSR-entered during the repro) -- fixed with a targeted `is_osr_denied()` static deny for these two methods pending a full x64-codegen root cause.
- OPEN (new, split out): [`testmonotonicity-quantile-cdf-dispatch-performance.md`](elasticsearch-suite/testmonotonicity-quantile-cdf-dispatch-performance.md) -- `testMonotonicity` (masked by the two bugs above, which used to abort it almost immediately) now runs to genuine completion but takes 900+ seconds (`-Jit on`) or doesn't finish in an hour (`-Jit off`) for a 10,001-point quantile/cdf sweep; confirmed real forward progress (not a livelock) via live `gdb` sampling repeatedly landing in `find_method_recursive`/`resolved_private_invokevirtual_target` mid `HashMap::insert`/`reserve_rehash`, suggestive of a per-call resolution cache that isn't being hit.

## 2026-07-10 ES vector-codec exception/cause-object family RETIRED (3 stacked fixes); s2-ByteBuffer real-JDK gaps split out

- FIXED/RETIRED: [`ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object-FIXED.md) вЂ” all 3 family rows green (seed `B17AC9D3E1F2A0C4`, JIT-on and `--nojit`). Three stacked bugs on top of the already-retired `RandomizedContext.current()` regression: (1) the live s2 `ByteBuffer.put(ByteBuffer)` native silently no-oped when either side is a real-JDK direct buffer вЂ” `IOUtil.read` hands every buffered `FileChannel` read through a temporary direct buffer, so FS-directory reads returned correct counts while delivering ZERO bytes and `BufferedIndexInput` underflowed on the first `readByte()`; (2) `Throwable.addSuppressed` wrote its suppressed array into positional field 2 = real-JDK `cause` вЂ” the `Caused by: java.lang.Object` corruption (this refutes the GC self-forward-relocation theory from the earlier update: the `CRATONVM_DBG_CAUSE` write was never observed because the clobber bypassed `write_throwable_cause` entirely); (3) `s2_bb_order` slot-5 aliasing ignored `order(LITTLE_ENDIAN)` on real-JDK buffers, byteswapping buffered `getShort`/`getInt`/`getLong` (unmasked by #1: `truncated file: length=79 but expectedLength==5692549928996306944`, i.e. 79 byte-reversed).
- ~~OPEN~~ FIXED/RETIRED 2026-07-13 (moved to `docs/internal/`): [`fixed-suite-bugs/s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md`](../internal/fixed-suite-bugs/s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md) вЂ” remaining real-JDK gaps in the s2 ByteBuffer native family found by inspection during the fix (bulk `get`/`put` on direct receivers, `equals`/`hashCode`/views, a `ChecksumIndexInput.getChecksum()` value divergence vs HotSpot on identical bytes, and a `ByteBuffersDirectory` reflective `<init>` gap under forced `-Dtests.directory`).

## 2026-07-10 FormAuthenticator `StreamDecoder` field-index residual FIXED; new zero-byte HTTP request blocker filed (unrelated, pre-existing)

- FIXED/RETIRED: [`form-authenticator-cookie-session-bare-assertion-FIXED.md`](../internal/fixed-suite-bugs/tomcat/form-authenticator-cookie-session-bare-assertion-FIXED.md) вЂ” the doc's last open residual (`NoSuchMethodError: java/lang/Object.read([CII)I` / `BufferedReader.in` reading null right after construction) was `alloc_stream_decoder` addressing the real `sun.nio.cs.StreamDecoder`'s `in` field and its side-table key via hardcoded absolute slot indices (`SD_INPUT = 0`, `SD_ID = 4`) that didn't match the real class's actual declared field order (`javap`: `closed, haveLeftoverChar, leftoverChar, cs, decoder, bb, in, ch` вЂ” `in` is the 7th field, slot 0 is really `closed`, a primitive) вЂ” the same "synthetic native writes the wrong slots for a real-`ClassId`-stamped object" corruption family as `LinkedBlockingDeque`/`ThreadPoolExecutor`/`StringJoiner`. Fixed the same way `stream_encoder.rs` already fixed the analogous `StreamEncoder` corruption: resolve `in` by name (`get_field_by_name`/`set_field_by_name`) instead of a hand-counted index, and key the side-table by `ctx.identity_hash_code` instead of a scratch field slot. Verified via `cargo test -p cratonvm-native-io stream_decoder::` (16/16) and two live `TestFormAuthenticatorA/B/C` reruns showing zero `NoSuchMethodError`/`lock is null` occurrences (commit `e5f20c9bf`).
- OPEN (new, pre-existing, unrelated вЂ” found while re-verifying the above): [`formauth-zero-byte-garbage-http-request.md`](tomcat/formauth-zero-byte-garbage-http-request.md) вЂ” every `TestFormAuthenticatorA/B/C` method's first request now fails with the server reading a few hundred NUL bytes instead of an HTTP method line, on a freshly-accepted socket. Confirmed NOT caused by the fix above via a same-dev-tip baseline comparison (an unmodified binary hit the identical symptom, and fared worse вЂ” hung instead of completing). Root cause not narrowed; may be host-load/contention on this specific run rather than an always-reproducible VM bug вЂ” needs a rerun on an idle host first.

## 2026-07-10 zero-byte HTTP request blocker RULED OUT (host-load artifact, confirmed on idle host); TestFormAuthenticatorA/B/C now pass end-to-end after merging 3 unrelated concurrent fixes

- RULED OUT: [`formauth-zero-byte-garbage-http-request-RULED-OUT.md`](../internal/fixed-suite-bugs/tomcat/formauth-zero-byte-garbage-http-request-RULED-OUT.md) вЂ” re-tested the doc's exact `TestFormAuthenticatorA/B/C` repro on a comparatively idle Azure Linux host (vs. the original Windows box at ~99% disk full under heavy concurrent load). 7 reruns (3Г— A, 2Г— B, 2Г— C) of the identical Java-level test methods against a freshly built dev-tip binary show **zero** NUL-byte/"Invalid character found in method name" occurrences вЂ” confirms the doc's own top open question (host-load artifact vs. real VM bug) in favor of the former. Retired as a non-issue. Important caveat, documented in the retired doc: this rerun required an unrelated one-off `-Dtomcat.test.basedir=.../-Dtomcat.test.tomcatbuild=...` invocation flag to work around a pre-existing Linux-fixture gap (missing `webapps` symlink under `/data/data/apps/tomcat`), so it is not a byte-for-byte repro of the original PowerShell/Windows invocation вЂ” a different, narrower issue than the one this doc tracked.
- With the zero-byte symptom gone, all three classes' remaining failures converged on one cause вЂ” `NullPointerException: Cannot invoke "java.io.FileDescriptor.closeAll(java.io.Closeable)" because "this.fd" is null` during JSP compilation, aborting the compile and leaving `FormAuthenticator` unable to forward to the login page. A duplicate-fix check before filing a new doc found this was **already independently fixed** a few commits ahead on `origin/dev` by the `TestJspDocumentParser` investigation above (`deb38efc`/`jspdocumentparser-saxparse-malformed-markup-FIXED.md`, same file, same `native_fis_close` slot-0 clobber). Merged that fix in; reran вЂ” zero recurrences. No new doc filed.
- After merging that + 24 other concurrent commits, hit a new, unrelated segfault (SIGSEGV) in all three classes partway through the run. A second pre-push `git fetch`/merge turned up the fix, also from a concurrent session on an unrelated ES investigation: `93b33576` ("Fix guarded-inline-getfield SIGSEGV regression masking ES IVF-KNN vector hang cluster") flips a buggy default-on JIT fast path (`guarded_inline_getfield`, introduced by `07dfa5e0`) back to opt-in. Merged that in, rebuilt, reran: `TestFormAuthenticatorB`/`C` pass cleanly (`OK (6)`/`OK (7)`); `A` passed cleanly on a retry after one host-load-flaky timeout (`OK (9)`). **End state: all three classes now pass end-to-end on this host**, after pulling in three unrelated fixes from three different concurrent sessions (none written by this investigation) plus ruling out the zero-byte symptom itself.

## 2026-07-10 ES suite-wide RandomizedRunner CCE FIXED (bisected to `aa21e334`); new pre-existing StringJoiner content bug filed

- FIXED/RETIRED: [`ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce-FIXED.md) вЂ” every `RandomizedRunner`-based ES test class failed at bootstrap with `ClassCastException: ArrayList cannot be cast to String[]` in `Modifier.toString`/`StringJoiner.add`, blocking the whole suite. Bisected to dev `aa21e334` ("Fix Spring SpEL evaluation edge cases"), which made `StringJoiner` yield to real bytecode for the first time at the interpreter's own dispatch loop вЂ” exposing a deterministic heap-reference-integrity defect (`gen_heap::read_slot` "corrupt Value cell"/`HIB-CV-32` guard) in real `StringJoiner.add()`'s `elts[size++]=elt` bytecode pattern that does not reproduce for an equivalent user-defined class (ruled out via two standalone `MicroProbe` repros). Fixed by excluding `java/util/StringJoiner` from the new dispatch check's allowlist, reverting only that one class at that one dispatch point back to its proven-safe pre-`aa21e334` behavior (`vm/src/vm/vm_exec.rs`'s separate, older allowlist for the same class is untouched). Verified: the doc's exact repro (6 classes) now 6/6 PASS; a 60-class broader sweep matches a prior session's pre-regression baseline byte-for-byte (zero new failures).
- FIXED/RETIRED (2026-07-11, pre-existing, unrelated to the above): [`stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md`](../internal/fixed-suite-bugs/stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md) вЂ” `StringJoiner`'s `SyntheticStub` native used a legacy 5-field layout that didn't match the real JDK's actual 7-field layout, so in real-JDK mode it silently produced wrong content (`Modifier.toString()`/any `StringJoiner.toString()` returned `""` instead of the joined string) вЂ” confirmed present identically on dev `4b08ffad`, well before `aa21e334`, i.e. not a regression from that commit. Fixed by resolving the real class's field indices by name and reimplementing `add`/`toString`/`length`/`merge`/`setEmptyValue` against the real 7-field layout (falling back to the untouched legacy path in synthetic-JDK mode); verified byte-for-byte against HotSpot including a reflection field dump.
- FIXED/RETIRED (same root cause, found concurrently by a different session): [`junit-consolelauncher-picocli-classcastexception-arraylist-stringarray-FIXED.md`](../internal/fixed-suite-bugs/junit-consolelauncher-picocli-classcastexception-arraylist-stringarray-FIXED.md) вЂ” identical `ArrayList`в†’`String[]` CCE via `StringJoiner.add`в†ђ`Modifier.toString`в†ђ`Field.toGenericString`, reached through picocli's `CommandLine$Model$TypedMember.getToString` instead of `RandomizedRunner`'s `ClassModel`. That session's bisection window `(8cdc1c011..c3c2b9ee2]` contains `aa21e334`, confirming the same root cause; not independently re-run against its own commons-math/picocli repro (no commons-math test classpath available on the collection host), so reopen if that specific repro still fails.

## 2026-07-10 Executors factory mainlock NPE FIXED (layer 2); two unrelated dev regressions found while verifying

Fixed the layer-2 residual left open in the `Executors.new*ThreadPool()` mainlock/ctl NPE doc, and
found two independent, pre-existing regressions on dev while trying to verify it suite-wide.

- FIXED/RETIRED: [`ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md) вЂ” `Executors.newFixedThreadPool`/`newCachedThreadPool`(x2)/`newSingleThreadExecutor` now drive the real `ThreadPoolExecutor(...)` constructor (`initialize_real_thread_pool_executor`, mirroring the existing `ScheduledThreadPoolExecutor` real-init pattern) instead of a fake 2-field layout, and `ThreadFactory.newThread(Runnable)` now drives the real `Thread(Runnable)` constructor instead of a fake 5-field layout (the latter meant a real pool's worker never actually ran submitted tasks even once the pool itself became real). Verified via an extended standalone probe (all 4 factories, custom `ThreadFactory`, `submit`/`execute`/`shutdown`/`shutdownNow`/`awaitTermination`, `shutdownNow()` correctly interrupting a blocked worker) and the ES `storedscripts` cluster (8/9 classes now pass; the 9th has an unrelated pre-existing serialization bug).
- FIXED/RETIRED: [`threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor-FIXED.md`](../internal/fixed-suite-bugs/threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor-FIXED.md) вЂ” filed independently and concurrently alongside `306cd352`'s `execute()`-dispatch fix; resolved as a side effect of the above, since these objects are now genuinely real and that fix's own receiver-aware checks correctly treat them as such.
- OPEN (new, blocking suite-wide verification): [`elasticsearch-suite/ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce.md`](elasticsearch-suite/ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce.md) вЂ” every `RandomizedRunner`-based ES test class (i.e. essentially the whole suite) now fails at bootstrap with `ClassCastException: ArrayList cannot be cast to String[]` in `Modifier.toString`/`StringJoiner.add`, before any test method runs. Bisected to dev commit `aa21e334` ("Fix Spring SpEL evaluation edge cases"); absent at the immediately-prior commit `4b08ffad`. Unrelated to the fix above (reproduces identically with or without it).
- FIXED/RETIRED: [`threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md`](../internal/threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md) вЂ” root cause was the fourth, unpatched dispatch path this doc's own analysis suspected: `try_stackless_invoke`'s direct native lookup. Fixed the same day by generalizing the registry-drop-removal approach (see the `ThreadPoolExecutor` regression entry below) вЂ” verified with this doc's own `ExecProbe3.java`, now printing 3 distinct worker threads instead of one.

## 2026-07-10 TestEncodingDetector fully green: UTF-16/prolog-conflict residual retired

- FIXED/RETIRED: [`encodingdetector-utf16-and-conflicting-prolog-residuals-FIXED.md`](../internal/fixed-suite-bugs/encodingdetector-utf16-and-conflicting-prolog-residuals-FIXED.md) вЂ” both residual clusters traced to the same root cause: synthetic `BufferedInputStream`/`InputStreamReader` overrides (added in `45cc4f4f`) unconditionally shadowed real JDK 25 bytecode for every instance, not just genuinely-synthetic-stub ones, because the interpreter's `invokevirtual` vtable fast path doesn't consult the `NativeKind::SyntheticStub` category the way `vm_exec.rs`'s `real_protected_stub` check does. `BufferedInputStream.reset()` was a silent no-op (broke `EncodingDetector`'s mark/reread-with-detected-encoding sequence); `InputStreamReader.read([CII)I` ignored the charset entirely (broke every UTF-16BE/LE decode). Removed both native overrides вЂ” real bytecode already implements them correctly. `org.apache.jasper.compiler.TestEncodingDetector`: `OK (22 tests)`, matching HotSpot exactly.

## 2026-07-10 WildFly corrupt-Value doc: `Level.parse` FIXED, new `ThreadPoolExecutor.execute()` regression found (OPEN, blocking)

Investigating `wildfly-domain-heap-corrupt-value-timeout.md`'s front-line
residuals surfaced two unrelated, earlier-gating bugs before those residuals
could be reached again:

- FIXED: [`java-util-logging-level-parse-throws-for-all-names-FIXED.md`](../internal/fixed-suite-bugs/java-util-logging-level-parse-throws-for-all-names-FIXED.md) вЂ” `java.util.logging.Level.parse(String)` threw `IllegalArgumentException` for *every* name, including standard JDK constants (`Level.parse("WARNING")` itself failed), due to a JDK-25 `KnownLevel`/module-synthesis gap. Broke WildFly's own `host.xml`/`domain.xml` parsing of `<level name="WARN"/>`. Fixed with a targeted native override.
- FIXED/RETIRED: [`threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`](../internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md) (execute) + [`threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor-FIXED.md`](../internal/fixed-suite-bugs/threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor-FIXED.md) (submit/shutdown) + [`threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md`](../internal/threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md) (real-executor async semantics) вЂ” `Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/`newCachedThreadPool()` return objects whose `.execute(Runnable)`/`.submit(...)`/`.shutdown()` used to NPE on `ThreadPoolExecutor`'s uninitialized `ctl`/`mainLock` fields, and separately any real `ThreadPoolExecutor.execute()` had degraded to synchronous dispatch вЂ” root cause was a registration-time drop in `native-api/src/registry.rs` (not the originally-bisected `f28d6ae6`; that was a red herring) that starved these synthetic objects' native overrides, compounded by a fourth, unpatched dispatch path (`try_stackless_invoke`) once a narrower fix tried to make the drop receiver-aware. Three overlapping fixes landed the same day from parallel sessions (`fix/tpe-npe-dispatch-20260710` for `execute()`'s NPE, this session's own Executors-real-init fix for `submit`/`shutdown`, then `fix/wildfly-hib32-gate-20260710` generalizing the dispatch fix and closing the async-semantics gap); see the FIXED docs for the reconciliation.

## 2026-07-10 Tomcat NIO/HTTP2 bare-assertions doc RETIRED; ByteBuffer.mark()/reset() found broken for real-JDK objects (FIXED); 1 narrow residual split off

Fixed the doc's own `\p{XDigit}` regex residual, and вЂ” while root-causing
the other two вЂ” found and fixed an unrelated, much bigger bug:
`ByteBuffer.mark()`/`reset()` were completely broken for real-JDK
`ByteBuffer`/`DirectByteBuffer` objects (`InvalidMarkException` on every
`reset()`, even right after a matching `mark()`; SIGSEGV on direct buffers
after the first `mark()` call). This explained both the `TestHttp2Limits`
regression and the previously-unexplained Jasper JSP failure in
`TestHttp11Processor`.

- FIXED/RETIRED: [`nonblockingapi-http11processor-http2limits-bare-assertions-FIXED.md`](../internal/fixed-suite-bugs/nonblockingapi-http11processor-http2limits-bare-assertions-FIXED.md) вЂ” 5/6 of the doc's originally-failing methods now pass (`testDelayedNBWrite`, `testPipelining`, `testWithTEChunkedWithCL`, `testHeaderLimits100x32`, `testPostWithTrailerHeadersSize0`); root cause and fix for both the regex gap and the `ByteBuffer` bug are in the doc's final section.
- OPEN (new, split off): [`tomcat/nonblockingreadignoreisready-async-error-response-completion-gap.md`](tomcat/nonblockingreadignoreisready-async-error-response-completion-gap.md) вЂ” `TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady`'s Java-level `onError`/`onComplete` callback sequence is confirmed byte-for-byte identical to HotSpot (via socket-capture + log diff), but CratonVM then writes zero bytes to the socket where HotSpot's container commits an implicit `200` response. Narrowed but not root-caused; low priority (narrow, deliberately-adversarial test scenario).

## 2026-07-10 ES storedscripts crash trio FIXED; Object.contains signal gone (masked); new Executors factory mainLock NPE found (OPEN, partially fixed)

Re-verified the 3 `findNative`-crash-family docs for
`org.elasticsearch.action.admin.cluster.storedscripts.{GetScriptContextResponseTests,GetStoredScriptResponseTests,ScriptContextInfoSerializingTests}`
on current `dev` per their own note ("re-run on current dev before assigning ownership"). All three crashes are CONFIRMED fixed (now `rc=1`, all 8 tests parsed instead of `rc=139`/0 parsed), and investigating `ScriptContextInfoSerializingTests`'s extra `Object.contains`/`Objects.equals` dispatch signal surfaced an unrelated, real bug:

- FIXED/RETIRED: [`ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getscriptcontextresponsetests-0b8ba47253-FIXED.md`](../internal/fixed-suite-bugs/ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getscriptcontextresponsetests-0b8ba47253-FIXED.md), [`...getstoredscriptresponsetests-1a2b5efa27-FIXED.md`](../internal/fixed-suite-bugs/ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getstoredscriptresponsetests-1a2b5efa27-FIXED.md), [`...scriptcontextinfoserializingtests-e2cdec4d58-FIXED.md`](../internal/fixed-suite-bugs/ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-scriptcontextinfoserializingtests-e2cdec4d58-FIXED.md) вЂ” the same already-fixed `findNative`/Panama crash family as the tdigest retirement below; rebuilding `dev` and rerunning each doc's repro (index adjusted for `others.tsv` drift вЂ” see the docs) confirms no crash and no `System$1.findNative` `NoSuchMethodError`.
- `ScriptContextInfoSerializingTests`'s extra signal (`NoSuchMethodError method="java/lang/Object.contains(...)Z" caller="java/util/Objects.equals(...)"`, on `testConcurrentEquals`/`testConcurrentToXContent`) does **not** reproduce on current `dev` вЂ” but this could not be cleanly confirmed as "fixed" rather than "masked": both affected tests now die *earlier*, in a newly-identified, unrelated `Executors` factory bug (next bullet), before ever reaching the `a.equals(b)` call site that produced the original signal. Static review of the vtable/dispatch fast paths (`vm/src/runtime/vtable.rs::lookup_slot`, `vm/src/runtime/interpreter.rs::execute_invokevirtual_{vtable_fast,cached}`) found the receiver-class/name/descriptor verification intact on all of them, and the original signal fired shortly after the (now-fixed) `findNative` crash in the same process, consistent with it having been collateral of that crash cascade rather than a standing bug вЂ” but this is not proven with a clean direct re-exercise. See the `ScriptContextInfoSerializingTests` FIXED doc above for the full writeup; should it resurface once the executor bug below is fixed, it needs its own fresh repro.
- FIXED/RETIRED: [`ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md`](../internal/fixed-suite-bugs/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md) вЂ” all 3 classes' `testConcurrentSerialization`/`testConcurrentHashCode`/`testConcurrentEquals`/`testConcurrentToXContent` (from `AbstractWireTestCase`) used to fail with `NullPointerException: ... because "mainLock" is null`. Layer 1 (class-tag mistagging) was fixed the same session this doc was filed; Layer 2 (the `drop_real_layout_synthetic` registration-time gate) is now fixed too вЂ” see the FIXED doc for the cross-reference to the actual fix.

## 2026-07-10 TestEncodingDetector retired; UTF-16/prolog-conflict residual split off

- FIXED/RETIRED: [`encodingdetector-jsp-encoding-500-failures-FIXED.md`](../internal/fixed-suite-bugs/encodingdetector-jsp-encoding-500-failures-FIXED.md) - the StAX prolog-encoding fix (`1b60c103`) was already merged; verifying it end-to-end against the real `TestEncodingDetector` class required also picking up a concurrent session's fix for two independent blockers (`FileInputStream.<init>(String)` native-fallback backfill gap; `defineClass1` duplicate-define during repeated Tomcat webapp stop/start in one process, commit `45cc4f4f`). With both merged, the class went from 22/22 failing to 5/22 failing.
- OPEN (new, split off): [`tomcat/encodingdetector-utf16-and-conflicting-prolog-residuals.md`](tomcat/encodingdetector-utf16-and-conflicting-prolog-residuals.md) вЂ” the remaining 5/22: 3 deliberately-invalid BOM/prolog-conflict fixtures now return 200 instead of HotSpot's 500, and 2 plain-`.jsp` UTF-16 (no-prolog) cases either decode garbled or hang.

## 2026-07-10 ES tdigest SortingDigestTests crash FIXED, 2 new correctness residuals found (OPEN)

Re-verified `ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511.md` on current `dev` per its own note ("re-run on current dev before assigning ownership"). The crash is CONFIRMED fixed, and with it gone the tests now run far enough to expose two independent, previously-hidden bugs:

- FIXED/RETIRED: [`ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511-FIXED.md`](../internal/fixed-suite-bugs/ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511-FIXED.md) вЂ” `JavaLangAccess.findNative(ClassLoader, String)J` is already registered on `java/lang/System$1`/`JavaLangAccess` with the exact crashing descriptor; rebuilding `dev` and rerunning the doc's exact repro confirms the `NoSuchMethodError` and `rc=139` crash no longer occur (now `rc=1`, all 20 tests parsed).
- OPEN (new): [`ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness.md`](elasticsearch-suite/ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness.md) вЂ” `SortingDigestTests` still FAILs 6/20 tests, but with **different failures depending on `-Jit on` vs `-Jit off`**: JIT-on shows wrong quantile values plus an `ArrayIndexOutOfBoundsException` on an identical garbage index (`7598259162470311681`) recurring across two unrelated arrays (smells like one stale 64-bit slot read as an index); JIT-off instead shows a `NoSuchMethodError: java/lang/Object.get(I)D` (receiver-identity loss dispatching a lambda passed as `java.util.function.Function`) and a repeated `ClassCastException` on log4j's `ReusableParameterizedMessage`. Neither cluster is root-caused yet.

## 2026-07-09 Tomcat AsyncContext JULI LogManager note retired

- FIXED/RETIRED: [`asynccontext-logmanager-getlogger-null-FIXED.md`](../internal/fixed-suite-bugs/asynccontext-logmanager-getlogger-null-FIXED.md) - `LogManager.addLogger(Logger)` now indexes real-JDK/JULI logger objects by their real `name` field instead of assuming synthetic slot 0 contains the name. The focused native regression passes, and a uniquely named Azure release binary (`/data/data/cratonvm-probes/bin/cratonvm-tomcat-async-logmanager-20260709`) passes a real-VM probe that performs the Tomcat-shaped `addLogger` -> `getLogger(name)` -> `setLevel` sequence for `org.apache.catalina.core.AsyncContextImpl`. The full Tomcat runner was unavailable on the host because the backup fixture lacked `test/` and `output/` artifacts, so future suite reruns should treat this as fixture confirmation rather than an open VM mechanism.

## 2026-07-09 Tomcat WebSocket close-delay repro blocked by 3 environment bugs (2 fixed, 1 new open)

While chasing `wsremoteendpoint-close-delay-near-deadlock.md`, found the
real-JDK-mode Tomcat repro no longer starts at all (regression vs. the
2026-07-07 evidence in that doc). Root-caused and fixed two of three
blockers; the third is a new open issue:

- FIXED/RETIRED: [`file-fs-native-clinit-never-set-FIXED.md`](../internal/fixed-suite-bugs/file-fs-native-clinit-never-set-FIXED.md) вЂ” `native_file_clinit` (the native override for `java/io/File.<clinit>`) never set the `FS` field, so any real-bytecode `File` method not in the `check_override` allow-list (`isInvalid()` and everything built on it вЂ” `length()`, `delete()`, `mkdir()`, `list()`, вЂ¦) NPE'd. 100% reproducible; broke Tomcat's `Digester`/`mbeans-descriptors.xml` bootstrap on every real-JDK-mode `Tomcat.start()`.
- FIXED/RETIRED: [`threadgroup-native-field-index-mismatch-FIXED.md`](../internal/fixed-suite-bugs/threadgroup-native-field-index-mismatch-FIXED.md) вЂ” the native `java.lang.ThreadGroup` accessors used a stale/swapped field-index layout (`name`/`parent` swapped vs. the real JDK 25 class layout), corrupting every VM-bootstrapped `ThreadGroup`. Broke `jdk.internal.misc.InnocuousThread.<clinit>` (`Cleaner.create()`) with a `ClassCastException`, failing `StandardServer` init before any application code ran.
- OPEN (new): [`enumset-of-broken-for-non-jdk-enums.md`](enumset-of-broken-for-non-jdk-enums.md) вЂ” `EnumSet.of(...)` silently returns a broken/empty, non-iterable set for non-JDK enums (e.g. `jakarta.servlet.DispatcherType`). Current blocker: Tomcat's `WsServerContainer` constructor uses this exact pattern for filter-dispatcher-type registration, so every websocket-enabled `StandardContext` fails to start. Root-cause narrowed to two candidates in the doc, not yet fixed.

The original websocket close-delay bug itself remains OPEN and
unconfirmed at the I/O level вЂ” see the doc's 2026-07-09 addendum for the
strengthened (but not yet empirically verified) root-cause hypothesis
(a bounded 20s blocking-send timeout expiring rather than a permanent
deadlock, `CountDownLatch` ruled out, socket write-readiness path now the
leading suspect) and the concrete next steps once the `EnumSet` blocker
above is cleared.

## 2026-07-09 AccessLogValve/RewriteValve re-verify: 2 severe regressions FIXED, 1 new foundational bug found (OPEN)

Re-verified `tomcat/accesslogvalve-rewritevalve-connection-failures.md`
in isolation (`-Parallel 1`, idle Azure host) per its own recommendation.
Both classes are CONFIRMED genuine bugs, not contention. Investigating them
surfaced three distinct, layered issues:

- FIXED: `URL.openConnection()` returned the wrong carrier type
  (`ClassCastException`) for any real http(s) URL, due to a field-5
  (authority) parsing bug introduced by commit `b0dd2e72` (2026-07-07).
  Huge blast radius вЂ” anything doing `(HttpURLConnection)
  url.openConnection()` on a real-bytecode URL was broken. Fixed in
  `net_phase_e.rs`, verified byte-identical to HotSpot.
- FIXED/RETIRED: [`bytebuffer-address-unset-aioobe.md`](../internal/fixed-suite-bugs/tomcat/bytebuffer-address-unset-aioobe.md)
  ? `ByteBuffer.allocate()`'s synthetic carrier did not set
  `Buffer.address`, so any bulk `get(byte[])`/`put(byte[])` threw
  `ArrayIndexOutOfBoundsException` via `Unsafe.copyMemory`. The live
  default-release allocator is `native-builtins/src/lib.rs::alloc_heap_bytebuffer`;
  it now seeds `address = 16`, and the socket-read/bulk-get repro returns
  `HTTP/1.1 200 OK`.
- FIXED/RETIRED: [`stringreader-read-never-advances-infinite-loop-FIXED.md`](../internal/fixed-suite-bugs/stringreader-read-never-advances-infinite-loop-FIXED.md)
  вЂ” `StringReader.read()` never advanced position, infinite-looping any
  `BufferedReader`/`StringReader`-based text parser (e.g.
  `RewriteValve.parse()`). Root cause: the live native (`native-io`'s
  `register_string_rw_natives`, tagged `SyntheticStub`) wins dispatch over
  real bytecode by default, but stored position/length in flat object field
  slots that don't exist on real JDK 25's `StringReader` (rewritten to a
  single `Reader` delegate) вЂ” the writes silently no-op'd. Fixed with a
  GC-stable side table (`SR_STATE`, keyed by `identity_hash_code`), same
  pattern as this file's `InputStreamReader` `ISR_PENDING` table.
  `TestRewriteValve` now completes all 121 tests instead of hanging.
- Updated: [`tomcat/accesslogvalve-rewritevalve-connection-failures.md`](tomcat/accesslogvalve-rewritevalve-connection-failures.md)
  now reflects all three findings.

## 2026-07-09 Spring suite genuine-bug list, updated (125, down from 159)

- [`CRATONVM-SPRING-GENUINE-BUGLIST-125.md`](CRATONVM-SPRING-GENUINE-BUGLIST-125.md) вЂ” full per-test-method detail for 125 CratonVM-unique Spring failures (HotSpot passes, CratonVM doesn't), cross-referenced against a clean HotSpot baseline with the classpath-dump gap fixed (spring-websocket/oxm/jms/orm/core-test jars were never built вЂ” `./gradlew jar testFixturesJar testClasses` fixed it). Down from 159 two dev commits ago: 65 newly fixed (entire SpEL cluster + spring-jms module), 31 "newly broken" are **not** new regressions вЂ” root-caused to the already-tracked HIB-CV-32 batch/load-dependent heap-corruption family (25/31 SIGSEGV, one test confirmed passing standalone but ABEND under full-suite load).

## 2026-07-09 BC-java `asn1-regression` X9Test SIGSEGV retired

- FIXED/RETIRED: [`bc-asn1-x9test-array-descriptor-of-checkcast-sigsegv-FIXED.md`](../internal/fixed-suite-bugs/bc-asn1-x9test-array-descriptor-of-checkcast-sigsegv-FIXED.md) - the interpreter now keeps popped `checkcast`/`instanceof` receivers pinned through the full type-check, including array descriptor handling, and the JIT `checkcast` helper rejects non-heap pointer-shaped receivers before reading object headers. `X9Test` now reports `X9: Okay`; the full ASN.1 regression run gets past X9 and only hits the unrelated `X500Name` Turkish-locale residual.

## 2026-07-09 BC-java `asn1-regression` StackOverflowError retired

- FIXED/RETIRED: [`bc-asn1-pkcs12test-indefinitelengthinputstream-stackoverflow-FIXED.md`](../internal/fixed-suite-bugs/bc-asn1-pkcs12test-indefinitelengthinputstream-stackoverflow-FIXED.md) - the base `InputStream.read([BII)` native no longer redispatches an explicit `super.read([BII)` call back to the receiver's three-arg override. The committed reduced probe covers the Bouncy Castle-shaped recursion and the earlier normal virtual-dispatch case; the local checkout does not include `apps/bc-java`, so full-suite rerun remains fixture validation rather than an open known issue.

## 2026-07-08/09 Hibernate remote rerun note retired

- FIXED/RETIRED: [hib-jpalargeblobtest-bulk-read-timeout-and-loaderr-artifact-FIXED.md](../internal/fixed-suite-bugs/hib-jpalargeblobtest-bulk-read-timeout-and-loaderr-artifact-FIXED.md) - re-auditing the raw remote logs showed the 7 post-`JpaLargeBlobTest` `LOADERR` rows were not same-process classloader poisoning; the runner was fork-per-class and also hit `No space left on device`. The real residual was `JpaLargeBlobTest`'s 200 MiB byte-at-a-time Blob stream path. The exact native fast path now preserves the fixture's `read`/`count` state and the remote two-class probe passes `JpaLargeBlobTest` in 3.501 s, with the following class reaching a normal result and no `loaderror`.

## 2026-07-08 New: `JettyClientHttpRequestFactoryTests` NPE (third distinct bug on this class)

- OPEN: [`jetty-clienthttprequestfactory-httpexchange-getrequest-npe.md`](jetty-clienthttprequestfactory-httpexchange-getrequest-npe.md) вЂ” `HttpExchange.getRequest()` NPE on a null `exchange`, 5 test methods. Found triaging a fresh-dev Spring non-passed rerun on Azure. Distinct from this class's two other already-fixed/tracked bugs (`StackOverflowError` dispatch cycle, NIO selector gap).

## 2026-07-08 NIO selected-key `ClassCastException` retired

- FIXED/RETIRED: [Tribes `ParallelNioSender` selected-key `ClassCastException`](../internal/fixed-suite-bugs/nio-selectionkey-classcastexception-tribes-sender-FIXED.md) - the remaining two-key `Selector.selectedKeys().iterator().next()/remove()` GC-stress crash was in native collection map/set helpers, not the selector side table. `HashMap.put`/`remove` now pin their inner receiver/key/value windows, `HashSet.iterator()` pins its receiver/snapshot backing before allocation, and the focused two-`SelectionKey` fixture passes under `CRATONVM_GC_STRESS`.

## 2026-07-08/09 Keycloak RealmModelTest protobuf metadata residual fixed; Liquibase no-JIT residual retired

- FIXED/VERIFIED: [`keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md) - real `RealmModelTest` under CratonVM `--nojit` no longer hangs. The run completed in 108.644s, reached `io.netty.channel.EventLoopGroup` `STOPPING` -> `STOPPED`, and did not emit the repeated STW takeover wait signature.
- FIXED: [`keycloak-model-protobuf-metadata-cache-config-missing-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-protobuf-metadata-cache-config-missing-FIXED.md) - real `DefaultCacheManager` cache-existence/configuration-definition calls now delegate into Infinispan's real `ConfigurationManager`, so the internal `___protobuf_metadata` cache configuration is present when internal caches start. The same fix chain also closed the exposed FFM `SymbolLookup`, default C-runtime lookup, StampedLock view-unlock, Liquibase pipeline-order, and Xerces `CMStateSet` hotspots.
- FIXED: [`keycloak-model-liquibase-xerces-xml-parse-nojit-timeout-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-liquibase-xerces-xml-parse-nojit-timeout-FIXED.md) - the no-JIT Liquibase/Xerces XML parse watchdog point is gone after native fast paths for XMLChar, XMLLimitAnalyzer, XSSimpleTypeDecl normalization, XSDKey identity, `XMLEntityScanner.scanQName`/content/space handling, opti DOM getters, and RangeToken sorting. The 2026-07-09 scanQName follow-up also fixed the downstream empty-rawname `skipString` failure and the exposed H2 `BitSet.clone`/`Object.clone` dispatch failure.
- FIXED/RETIRED: [`keycloak-model-liquibase-checksum-status-nojit-timeout-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-liquibase-checksum-status-nojit-timeout-FIXED.md) - after the checksum filter, H2 DDL hotpath, and `ColumnConfig.getSerializableFieldValue` shortcuts, `RealmModelTest` no longer hits the Liquibase checksum/status/update timeout or the earlier `SnapshotGeneratorFactory` comparator failure. The 2026-07-09 no-JIT validation ran all 195 Liquibase changesets and logged database-update completion before failing later.
- FIXED/RETIRED: [`keycloak-model-realmmodeltest-h2-auth-after-liquibase-nojit-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-realmmodeltest-h2-auth-after-liquibase-nojit-FIXED.md) - the later H2 auth/bootstrap residual is closed. The final Azure no-JIT run `verify-realmmodel-h2-auth-fixed-jdk25-20260709-123713-r109-final-candidate-700` passed `RealmModelTest` 3/3 in 352.304s with no H2 auth failure, no post-Liquibase timeout, and no localization `Map.forEach` NPE.
- FIXED/RETIRED: [`keycloak-model-realmmodeltest-post-infinispan-liquibase-timeout-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-realmmodeltest-post-infinispan-liquibase-timeout-FIXED.md) - the broader post-Infinispan tracker is closed by the same full-class no-JIT validation.

## 2026-07-07 Five new Spring non-passed-rerun bugs (Azure host, dev, real-JDK/JIT-on)

Found triaging FAIL results from a 516-class Spring non-passed rerun (filtered
out ~250 environmental classpath-dump gaps first вЂ” missing cross-module
Spring test-fixture jars, not CratonVM bugs). All 5 were genuine, distinct
CratonVM defects вЂ” **all 5 are now FIXED** (landed 2026-07-07/08, confirmed
by a follow-up rerun on fresh dev: `BufferingStompDecoderTests` 11/11 OK,
`MimeTypeTests` 47/47 OK, `MediaTypeTests` 28/28 OK,
`DefaultListableBeanFactoryTests.beanProviderSerialization()` no longer
throws). Retired here; docs moved to `docs/internal/fixed-suite-bugs`.

- FIXED/RETIRED: [`x509trustmanager-getacceptedissuers-abstractmethod-FIXED.md`](../internal/fixed-suite-bugs/x509trustmanager-getacceptedissuers-abstractmethod-FIXED.md) вЂ” `X509TrustManager.getAcceptedIssuers()` AbstractMethodError, identical across all 4 HTTP server backends in one WebFlux test.
- FIXED/RETIRED: [`groovy-compilationunit-phaseoperation-abstractmethoderror-FIXED.md`](../internal/fixed-suite-bugs/groovy-compilationunit-phaseoperation-abstractmethoderror-FIXED.md) - Groovy `CompilationUnit$PhaseOperation.doPhaseOperation` AbstractMethodError, 4 `GroovyScriptFactoryTests` methods.
- FIXED/RETIRED: [`stomp-bufferingdecoder-atomicinteger-count-npe-FIXED.md`](../internal/fixed-suite-bugs/stomp-bufferingdecoder-atomicinteger-count-npe-FIXED.md) вЂ” `BufferingStompDecoder`'s `count` `AtomicInteger` field null, 5 STOMP decoder test methods; real cause was `LinkedBlockingQueue.clear()` stomping the real-JDK field slot with a synthetic-layout write.
- FIXED/RETIRED: [`linkedcaseinsensitivemap-this0-deserialization-FIXED.md`](../internal/fixed-suite-bugs/linkedcaseinsensitivemap-this0-deserialization-FIXED.md) - `LinkedCaseInsensitiveMap` inner-class `this$0` null after deserialize; fixed by suppressing `removeEldestEntry` during `HashMap.readObject` replay.
- FIXED/RETIRED: [`reflection-arenestmates-native.md`](../internal/fixed-suite-bugs/reflection-arenestmates-native.md) вЂ” `jdk.internal.reflect.Reflection.areNestMates` had no native registered at all.

Also confirmed (not new, corroborating evidence only):
- FIXED/RETIRED: [`expression.spel.*` SpEL `EL1040E` double-literal-suffix parse failures](../internal/fixed-suite-bugs/spring-expression-double-literal-suffix-FIXED.md) - Java `Float.parseFloat`/`Double.parseDouble` type suffixes (`d/D/f/F`) now parse in native-builtins, closing the 33 suffix parse failures.
- OPEN corroborating evidence: `NoSuchMethodError: Object.accept/Object.test` in `SpelCompilerTests`/`BeanOverrideHandlerTests` matches the general JIT wrong-receiver-type/virtual-dispatch bug called out in `docs/internal/fixed-suite-bugs/jit-osr-linux-regression-triad.md` (that doc's top-level FIXED status covers only one narrow OSR-entry sub-case; this broader dispatch signature is explicitly noted there as still needing a general-path fix) - new evidence it also hits Spring, not just Hibernate.

## 2026-07-08 Hibernate bytecode-enhancement loader/lazytoone family retired; residual basic/merge/version bugs split out
## 2026-07-08 Hibernate bytecode-enhancement loader/lazytoone and residual basic/merge/version bugs retired

- FIXED/RETIRED: [`hib-bytecode-enhancement-loader-faithful-linking-FIXED.md`](../internal/fixed-suite-bugs/hib-bytecode-enhancement-loader-faithful-linking-FIXED.md) - the remaining loader-faithful lazy/lazytoone failures are closed: 18/18 representative sample and 69/69 lazy/lazytoone subset pass, and the graph same-name checkcast residual now passes.
- FIXED/RETIRED: [`hib-bytecode-enhancement-basic-merge-version-residuals-FIXED.md`](../internal/fixed-suite-bugs/hib-bytecode-enhancement-basic-merge-version-residuals-FIXED.md) - the six non-lazy basic dirty tracking, final-field embedded-id, composite merge/null, and versioned-entity residuals now pass.
## 2026-07-08 Keycloak RealmModelTest `fullName` note retired; H2 auth residual remains open

- FIXED/RETIRED: [Infinispan ProtoStream `FileDescriptor.fullName` decode error](../internal/fixed-suite-bugs/keycloak-model-infinispan-jit-adjacent-decode-error-fullname-FIXED.md) - the exact default-JIT `decode error at pc=51 in fullName...` no longer reproduces on current `dev`. The residual pass fixed the later real-`DefaultCacheManager.defineConfiguration` delegation gap, real-JDK `StampedLock` lock-view/native coherence, Windows C-runtime symbol lookup for Panama symbol lookup, and added a conservative RxJava3 JIT skip after `CRATONVM_JIT_DENY=io/reactivex/` proved it clears the Infinispan publisher wait.
- FIXED/RETIRED: [Keycloak `RealmModelTest` post-Infinispan tracker](../internal/fixed-suite-bugs/keycloak-model-realmmodeltest-post-infinispan-liquibase-timeout-FIXED.md) - the broad tracker is now closed by the 2026-07-10 no-JIT `RealmModelTest` pass; the active H2 auth follow-up moved to [`keycloak-model-realmmodeltest-h2-auth-after-liquibase-nojit-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-realmmodeltest-h2-auth-after-liquibase-nojit-FIXED.md).

## 2026-07-08 `InPredicateTest` LHM NSME and stale timeout notes retired

- вњ… FIXED: [`DomainParameterXref` `LinkedHashMap.removeEldestEntry` NSME](../internal/fixed-suite-bugs/hib-domainparameterxref-lhm-removeeldestentry-nsme-FIXED.md) вЂ” fresh Azure `dev@47bbdc3b` reproduced the slot-0/`java.lang.Object` class-id read and `Object.removeEldestEntry` NSME (`ok=0`, 33.697s). The LHM native now skips that impossible virtual call while preserving real subclass eviction hooks; the fixed probe still sees the slot-0 read but passes (`ok=1`, 55.971s).
- вњ… RETIRED: [`InPredicateTest` dispatch-heavy JIT timeout note](../internal/fixed-suite-bugs/hib-inpredicate-dispatch-heavy-jit-timeout-20260707-FIXED.md) вЂ” current recheck no longer times out. Before the LHM guard, current dev reached the later NSME in 33.697s; after the guard, the class passed under default JIT in 55.971s.
- вњ… RETIRED: [`ProxyClassReuseTest` / Spring Groovy residual cluster](../internal/fixed-suite-bugs/hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md) - the original Hibernate proxy case remains 3/3 green, and the residual Spring `GroovyBeanDefinitionReaderTests` (36/36), Spring `BshScriptFactoryTests` (18/18), namespace/component-scan probe, and related `InPredicateTest` default-JIT run all pass on the final 2026-07-08 Azure binary.

## 2026-07-07/08 crypto/fips1402 ProvEC `ClassNotFoundException`, SD-JWT hang, and P-384 KeyPairGenerator gaps FIXED

- FIXED: [ProvEC `AlgorithmParametersSpi$EC` class-not-found, crashing whole process](../internal/fixed-suite-bugs/keycloak-crypto-fips1402-provec-algorithmparametersspi-classnotfound-FIXED.md) - BouncyCastle-FIPS registers algorithms through a private `creatorMap` (`EngineCreator` factories), never a directly-loadable `className`; CratonVM's real-JCA bridge didn't know about this and tried (and, worse, non-catchably crashed on) reflectively loading BC-FIPS's cosmetic label string. Fixed by reaching `creatorMap` directly (`try_engine_creator_instantiate`) plus retaining the real `Provider` object across `Security.addProvider` (`real_provider_table`), and hardening the reflective fallback to raise a catchable exception instead of aborting the process.
- FIXED (same root cause/fix): [FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest hang after provider init](../internal/fixed-suite-bugs/keycloak-crypto-fips1402-sdjwt-hang-after-provider-init-FIXED.md) - was a 1200s HANG, now 16/16 PASS in ~117s.
- FIXED: [`KeyPairGenerator.getInstance("ECDSA","BCFIPS")` and no-provider SunEC P-384 keygen failures](../internal/fixed-suite-bugs/keycloak-crypto-fips1402-sunec-keypairgenerator-384bit-gap-FIXED.md) - explicit BC-FIPS EC/ECDSA keygen now returns the provider-created BC-FIPS generator through the existing `EngineCreator` bridge, and default SunEC keygen now bypasses the no-arg constructor's failing default-384 initialize before applying the caller's `ECGenParameterSpec`. Verified `BCFIPSECDSACryptoProviderTest` 3/3, `BCFIPSEcdhEsAlgorithmProviderTest` 2/2, and `FIPS1402SdJwtCreationAndSigningTest` 2/2.

## 2026-07-07 Three Keycloak nonpassed-rerun bugs FIXED (DependencyGraphResolver spin loop, Phaser/ForkJoinPool hang, Liquibase Scope corruption)

- вњ… FIXED: [`tests/base` `DependencyGraphResolver.scan()` spin loop](../internal/fixed-suite-bugs/tests-base-dependencygraphresolver-spin-loop-hang-FIXED.md) вЂ” missing early `return` after the "already scanned" check caused combinatorial re-traversal of shared DI dependencies, blowing a few-second HotSpot operation up into a 1200s CratonVM timeout. Fix lives in the vendored `apps/keycloak` Java source (test-framework/core), not CratonVM itself вЂ” needs re-applying to any other Keycloak checkout used as a suite fixture, or reporting upstream.
- вњ… FIXED (branch `fix/phaser-forkjoinpool-managedblock-hang-20260707`, commit `743da7b1`): [Phaser/ForkJoinPool hang in SmallRye Sisu bean loading](../internal/fixed-suite-bugs/keycloak-testframework-phaser-forkjoinpool-sisu-hang-FIXED.md) вЂ” `CompletableFuture.runAsync`'s native override swallowed `MethodCallFailed::InternalError` (VM-level, non-catchable-by-Java) without routing through the task's own exception table, so a VM-level gap deep in bean-loading skipped `finally { phaser.arriveAndDeregister(); }` and hung `arriveAndAwaitAdvance()` forever. `IdentityProviderMapperTest`: 12+ hour hang в†’ ~71s.
- вњ… FIXED (branch `fix/liquibase-scope-threadlocal-corruption-20260707`, commit `752796a0`): [Liquibase `Scope` "Cannot end scope ... at scope root"](../internal/fixed-suite-bugs/testsuite-model-liquibase-scope-corruption-FIXED.md) вЂ” a JIT `invokedynamic` uncommon-trap re-runs the whole method from the interpreter entry point instead of resuming from the throw point, double-executing any committing side effect (e.g. `Scope.enter()`'s field mutation) positioned before the indy call site. Fixed in `jit/src/x64.rs` (`jit_scan` now refuses to JIT-compile such methods). ThreadLocal/InheritableThreadLocal and try-with-resources-double-close theories were both investigated and ruled out first.

All three verified against their real Keycloak classes via the suite runner; no regressions in the touched crates' test suites.

## 2026-07-07 Hibernate local Windows rerun вЂ” retired aggregate note

- вњ… RETIRED: [hib-local-windows-rerun-20260707.md](../internal/hib-local-windows-rerun-20260707.md) вЂ” the 4-shard local rerun remains historical evidence for 15 confirmed fixes, `JpaLargeBlobTest`'s non-hang slow path, the superseded temporal-skew observation, and the harness status-computation false-positive. A 2026-07-08 Azure `dev@22d79b10` recheck with unique binary `/data/data/cratonvm-binaries/cvhiblocalcurrentdev-20260708-160310-rebased` passed `InPredicateTest` (`ok=1`, 60.643s) plus the fixed-bucket sample (`LocalXmlResourceResolverTest` 23/23, `ConfigurationTest` 1/1, `OrmXmlEnumTypeTest` 1/1), so this aggregate note no longer owns an open task.
- вњ… FIXED same day: the SQL-placeholder duplication that rerun re-confirmed (plus 2 more affected classes outside `type.temporal.*` вЂ” `ExtendedEnhancementNonStandardAccessTest`, `FunctionTests`, proving it a general SQL-generation-path defect) was the reopened JIT reason-8 imprecise-resume corruption; see [`docs/internal/hib-temporal-sql-parameter-placeholder-duplication-FIXED.md`](../internal/hib-temporal-sql-parameter-placeholder-duplication-FIXED.md) and the identity-sound precise-resume entry below.

## 2026-07-07 ES binary-docvalues range doc retired; young-GC RRWL residual fixed 2026-07-08

- [FIXED] [Young-GC live-object reclamation corrupting RRWL read-lock hold counts](../internal/fixed-suite-bugs/gen-heap-young-gc-live-object-reclaim-rrwl-holdcount-FIXED.md) - the `elasticsearch-lucene-binary-docvalues-range-hangs` residual and the standalone RRWL reader-vs-writer hang were traced through three layers: reference-processing survivorship, class-init missed-notify crawl, ThreadIdentifiers tid collisions, then the final Linux helper-window/native ClassLoader pinning gaps. The standalone `RwlReadTearingProbe` now completes 10/10 under `CRATONVM_DBG_GC_STRESS=200000` on the Azure validation host. Direct ES rerun remains unavailable there because the ES checkout/classpath is absent, so any future ES confirmation should be tracked as suite validation rather than keeping this fixed mechanism in `known-issues`.
- `elasticsearch-lucene-binary-docvalues-range-hangs.md` retired to
  [`docs/internal/`](../internal/elasticsearch-lucene-binary-docvalues-range-hangs.md): its three
  root causes (invokedynamic JIT blacklist, AQS skip-list gaps x2 rounds, plain-field 16-byte slot
  tearing) are all FIXED+merged, end-to-end verified on the Windows box. Its residual now points at
  the fixed RRWL document above.

## 2026-07-07 JIT invokedynamic uncommon-trap Groovy regression вЂ” FIXED (no tradeoff)

- вњ… FIXED: `fb4a333d`'s precise-resume routing for the invokedynamic
  uncommon trap (reason 8 / `UnreachedCode`) regressed
  `GroovyBeanDefinitionReaderTests` under JIT-on. An earlier pass shipped a
  blanket revert (reopening the exact silent-corruption risk `fb4a333d` had
  fixed) as a stopgap вЂ” rejected as a final answer. Follow-up investigation
  recovered `fb4a333d`'s own uncommitted standalone repros and found the REAL
  root causes: (1) `getstatic` codegen never marked a reference-typed static
  field as a GC/deopt oop (unlike `getfield`'s already-fixed inline arms),
  corrupting `LicmRepro`/`LicmRepro2`/`ArrRepro`; (2) the invokedynamic-trap
  snapshot mis-stamped its `DeoptReason` as `OsrExit` instead of
  `UnreachedCode`, so the trap was never blacklisted after firing; (3) three
  separate VM-side call sites consumed a stashed deopt frame without ever
  driving de-speculation, so a blacklisted method kept getting re-entered
  anyway. All four fixed; reason 8 keeps `fb4a333d`'s original unconditional
  precise-resume routing. Verified: the corruption-repro suite is now
  provably correct against HotSpot ground truth where it previously
  crashed; Groovy matches the pre-existing (already-merged) baseline exactly
  (30/36 isolated, 5/36 batch вЂ” both pre-existing numbers, unaffected either
  way); `cargo test -p cratonvm-jit --lib` unchanged (878 passed, 4
  pre-existing aarch64 failures). Doc moved to
  [`docs/internal/jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md`](../internal/jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md).

## 2026-07-09 GC_STRESS residual Fork6Hard lane - FIXED

- FIXED: the post-oldgen Fork6Hard `GC_STRESS` residual is closed by keeping
  the real-JDK ForkJoinPool/ForkJoinTask Bridge surface registered and forcing
  those bridge methods as a single side-table-backed model, plus GC scan/remap
  of the ForkJoin side table and root-snapshot cache bypass in the real-FJP
  lane. Verification on the Azure Linux probe host: default-JIT `Fork6Hard 128
  20` was 12/12 and 48/48 `ALL-OK`; `CRATONVM_DISABLE_JIT=1` was 12/12
  `ALL-OK`; all three lanes had 0 `mark_young` corrupt-header markers, 0
  timeouts, and 0 bad signatures. Full write-up moved to
  [gcstress-residual-corruption-faces-FIXED.md](../internal/gcstress-residual-corruption-faces-FIXED.md).

## 2026-07-06 Infinispan Cache.config null after real DefaultCacheManager.start() вЂ” FIXED; two new residuals found one/two layers deeper

- вњ… FIXED: `native_dcm_get_cache` (and all Cache-instance natives вЂ”
  `put`/`get`/`remove`/`containsKey`/`size`/`clear`/`evict`/`addListener`/
  `removeListener`/`getName`/`putIfAbsent`/`replace`) were registered
  unconditionally on `DefaultCacheManager`/`Cache`/`CacheImpl`/`AdvancedCache`,
  so they also intercepted a REAL manager's calls, fabricating a synthetic
  `Cache` with a null `config` вЂ” Infinispan's own internal bootstrap
  (`GlobalConfigurationManagerImpl.postStart()`) NPE'd on
  `cache.config.clustering()`. Fixed by guarding each with `is_real_dcm()`/a
  new `is_real_cache()` and delegating real objects to the real underlying
  bytecode (`internalGetCache`, `put(K,V,Metadata)`, `get(K,long,InvocationContext)`,
  etc. вЂ” found via `javap` decompilation of the real jar). `is_real_cache()`
  took three attempts to get right (two field-by-name discriminators both
  had value round-trip bugs on this class); see the FIXED doc for the full
  trail. Also fixed a latent GC-safety bug (bare `ObjectRef`/`Value` locals
  held across a re-entrant real-bytecode call in `get`/`containsKey`/`remove`)
  and a prerequisite Windows-only build regression (`native-io`'s
  `NativeSocketAddress` natives referenced `libc::sockaddr_in6`/`AF_INET`
  unconditionally, but `libc` doesn't define those for Windows вЂ” gated with
  `#[cfg(unix)]`). Verified: regression probe for the OLD synthetic path
  passes cleanly, all 21 pre-existing unit tests pass, and `RealmModelTest`'s
  originally-reported NPE is gone (gets through the entire cache-manager
  bootstrap/use/teardown lifecycle under `--nojit`). Doc:
  [`docs/internal/fixed-suite-bugs/keycloak-model-infinispan-cache-config-null-after-real-start-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-infinispan-cache-config-null-after-real-start-FIXED.md).
- The historical JIT-only ProtoStream `fullName` decode residual is now
  retired to
  [`docs/internal/fixed-suite-bugs/keycloak-model-infinispan-jit-adjacent-decode-error-fullname-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-infinispan-jit-adjacent-decode-error-fullname-FIXED.md).
  The deeper `RealmModelTest` follow-up is now retired to
  [`keycloak-model-realmmodeltest-post-infinispan-liquibase-timeout-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-realmmodeltest-post-infinispan-liquibase-timeout-FIXED.md).
  The sibling `--nojit` STW shutdown hang that surfaced alongside the old
  decode note is fixed and retired to
  [`docs/internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md).

## 2026-07-07 GC blocked-thread / stale-Thread-mirror doc RETIRED; real-net GC-blocking audit completed

- вњ… RETIRED to [`docs/internal/gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md`](../internal/gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md): the 2026-06-29 six-class Tomcat hard-crash family (blocked thread's live young object reclaimed в†’ SIGSEGV/`CompactValue` panic) is closed вЂ” 4297 encoding-suite Tomcat boot/serve/stop cases + all three tribes classes ran with ZERO stale-pointer warnings, ZERO panics, jit and nojit. Executing the doc's remaining audit TODO also landed six real-net GC-blocking fixes on this branch: `MulticastSocket.receive/send` and `DatagramChannel.receive0/send0` bracketted (+ held-ref re-sync), the `Selector.select` epoll wait bracketted (was the dominant STW-stall source вЂ” every NIO event loop held up every GC by its select timeout; `TestDataIntegrity --nojit` went from 420s-timeout/44 STW-stall warnings to completing with 0), `net_accept`'s post-wake writes re-synced, `net_connect0` bracketted, and `populate_selected_keys_field`'s `Set.add` loop pinned. The historical register-resident JIT oop remainder is now retired: DoHead moved to [dohead-jit-heap-corruption-register-invisibility-FIXED.md](../internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md), and Fork6/A4 moved to [fork6-fjp-multithread-jit-root-reclamation-FIXED.md](../internal/fixed-suite-bugs/fork6-fjp-multithread-jit-root-reclamation-FIXED.md).
- рџ”ґ NEW [tomcat-defaultservlet-encoding-content-failures.md](tomcat-defaultservlet-encoding-content-failures.md) вЂ” with the crash family gone, full 1360-case `TestDefaultServletEncoding{WithoutBom,WithBom}` nojit runs surface 264/228 functional failures (mostly `expected:<200> but was:<-1>`, exotic-output-encoding cases like ibm850 among the first failures; zero GC signal). Untriaged.
- вњ… FIXED, retired to [`docs/internal/nio-native-side-table-stale-objectref-FIXED.md`](../internal/nio-native-side-table-stale-objectref-FIXED.md) вЂ” 2026-07-07 audit: the doc's named `sk_table`/`key_obj` hazard was already covered when written (`gc_scan_selector_roots` + `sk_table_update_after_gc`, landed 2026-06-21, run on every moving-collection path). The audit found a REAL sibling bug in the same file: `build_set` (backing `Selector.selectedKeys()`/`.keys()`) built its `HashSet` via an unpinned Rust local across GC-capable `Set.add` calls вЂ” same shape as the already-fixed `populate_selected_keys_field`. Fixed + covered by a new targeted `CRATONVM_GC_STRESS` fixture (`vm/tests/nio_selector_build_set_gc.rs`); reproduced and confirmed gone against Tomcat Tribes `TestDataIntegrity`.

## 2026-07-07 Keycloak X.509 `AuthorityKeyIdentifier` NPE вЂ” FIXED, doc retired; new `tests/base` blocker found

- вњ… FIXED: `CertificateFactory.getInstance("X.509")`'s no-provider path (used by BC's default `JcaX509CertificateConverter`) returned certs whose `getEncoded()` was always a 0-length byte array, so any later BC ASN.1 re-parse (e.g. `createAuthorityKeyIdentifier`) NPE'd deep in `ASN1UniversalType.checkedCast`. This blocked ~80/101 `tests/base` classes plus 2 crypto-module tests. Fixed in `native-builtins/src/phases_late.rs`'s `CertificateFactory.generateCertificate`/`.generateCertificates` by building a real `sun.security.x509.X509CertImpl` from the DER (reusing `keystore::make_x509_mirror`) instead of a byte-discarding synthetic stub. Verified via isolated before/after repro (byte-identical to real HotSpot post-fix) and real Keycloak tests (`PemUtilsBCTest` 6/6 pass). Doc retired to [`docs/internal/fixed-suite-bugs/keycloak-x509-authoritykeyidentifier-npe-FIXED.md`](../internal/fixed-suite-bugs/keycloak-x509-authoritykeyidentifier-npe-FIXED.md).
- Fixing it far enough to verify surfaced the *next* `tests/base` blocker (every class hits this once the X.509 NPE is out of the way): `ProviderDeployer`/`org.keycloak.it.utils.Maven` fails to resolve the (locally-present) `keycloak-test-framework-remote-providers` artifact via its own reactor dependency-graph resolution. вњ… NOT A CRATONVM BUG вЂ” confirmed 2026-07-07 via direct HotSpot repro (identical failure, same stack trace, both VMs). Doc retired to [`docs/internal/keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md`](../internal/keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md).

## 2026-07-07 WP1.8 ServiceLoader iterator hang вЂ” FIXED, doc retired

- вњ… FIXED (root cause `8a61e6f8`, merged `1b131502`, 2026-07-06): the WP1.8 `ServiceLoader.load(Driver.class).iterator()` acceptance tests (`vm/tests/wp1_8_real_jar_serviceloader.rs`, `vm/tests/wp1_8_serviceloader_e2e.rs`) hung in an infinite `while (it.hasNext())` loop вЂ” the `ArrayList$Itr` fallback layout's lastRet slot collided with cursor (both slot 1), so `native_al_itr_next` overwrote its own cursor increment and `hasNext()` never went false on the natively-assembled provider list. Causally verified: the doc-era tree (`d9cb7be8`) hangs; the same tree + only the `8a61e6f8` native-collections patch passes in 0.01s; current dev passes 5/5 on Linux (real-JDK jdk25 + synthetic fallback) and Windows (jdk-25, the original report platform). Both `#[ignore]`s removed so the acceptance bar is enforced by default again; doc retired to [`docs/internal/wp1-8-real-jar-serviceloader-hang-FIXED.md`](../internal/wp1-8-real-jar-serviceloader-hang-FIXED.md).

## 2026-07-07 Hibernate temporal: GC crash family extinct в†’ new SQL-placeholder bug surfaced

- вњ… FIXED (branch `fix/hib-temporal-placeholder-dup-20260707`, 2026-07-07): the 5
  `type.temporal.*` classes' DUPLICATED JDBC `?` placeholders (`values (??,???)`) were NOT a
  string-building defect вЂ” they were the reopened JIT invokedynamic uncommon-trap
  imprecise-resume corruption (the `5ceb880f` revert of `fb4a333d`) re-executing
  JIT-committed `sqlBuffer` appends. Fixed by identity-sound precise resume for the reason-8
  trap (method-key-baked deopt snapshots + identity-checked consumers + dispatch-helper
  in-place callee resolution + direct-call/MIC/PIC publication gates), which closes BOTH this
  corruption AND the Groovy regression that had forced the revert. Doc retired to
  [`docs/internal/hib-temporal-sql-parameter-placeholder-duplication-FIXED.md`](../internal/hib-temporal-sql-parameter-placeholder-duplication-FIXED.md);
  the reason-8 saga's remaining follow-ups stay tracked in
  [jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md](../internal/jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md);
  the two unmasked PRE-EXISTING temporal residuals (37Г— `DdlTypeImpl.getRawTypeName` NPE in
  InstantTests, 2Г— empty `IllegalThreadStateException`) are now fixed by the conservative
  Hibernate JIT guard and retired to
  [hib-temporal-residuals-typename-npe-illegalthreadstate-FIXED.md](../internal/fixed-suite-bugs/hib-temporal-residuals-typename-npe-illegalthreadstate-FIXED.md).
  The later `LocalDateTimeTest` DST/H2 residuals are also fixed and retired to
  [hib-temporal-localdatetime-dst-h2-local4-residuals-FIXED.md](../internal/fixed-suite-bugs/hib-temporal-localdatetime-dst-h2-local4-residuals-FIXED.md).

## 2026-07-06/07 http.client class_manager RwLock recursive-read deadlock вЂ” FIXED (branch fix/class-manager-writer-starvation-20260706)

- FIXED and retired: the "writer starvation" residual (~25-50% `HttpComponentsClientHttpRequestFactoryTests` teardown hang surviving the `caa4ee65`/`60e2b20d` AB-BA fixes) was actually a same-thread RECURSIVE READ deadlock on `class_manager`: `try_stackless_invoke`'s native-override hierarchy walk held a read guard while `native_shadow_suppressed_by_redefine` re-acquired the same lock, and parking_lot's task-fair policy parks the nested read behind any queued writer (`load_class_concurrent`). Proved via debug-info gdb (lock state word `0x1b` = 1 held reader + parked writer; the nested acquisition was inlined and invisible in the original symbols-only captures). Fix: guard-reusing `native_shadow_suppressed_by_redefine_in(&cm, ..)`. 0/40 + 0/20 hangs post-fix (was 2 hangs in 3 attempts). Full writeup: `docs/internal/fixed-suite-bugs/class-manager-rwlock-recursive-read-deadlock-FIXED.md`.

## 2026-07-07 http.client Flow/Reactive hangs: root causes #1 and #2 FIXED, #3 partially fixed (TCP connect now succeeds, deeper hang remains)

- [spring-web-flow-outputstreamwriter-close-corruption.md](spring-web-flow-outputstreamwriter-close-corruption.md) -- вњ… root cause #1 FIXED (commit `6744812d`, merged `963d59b3`) and вњ… root cause #2 FIXED (commit `60bf9de0`, merged `86f37f84`): see prior entries, unchanged. рџџЎ Root cause #3 (`reactive.ClientHttpConnectorTests`, isolated to specifically `HttpComponentsClientHttpConnector` of the 4 parameterized connectors -- Reactor Netty/Jetty both work, Jdk fails fast with an unrelated `ClassCastException`) is **PARTIALLY FIXED**: commit `cf78cbb8` (merged `e84859d7`) fixed `SocketChannel`/`ServerSocketChannel.supportedOptions()` throwing `AbstractMethodError` (neither class had a native registration, so dispatch fell through to the abstract `NetworkChannel` declaration) -- this `Error` (not `Exception`) wasn't caught by HttpClient5's `catch (IOException | RuntimeException)` around connection setup, and `IOReactorWorker.run()`'s own `catch (Error e) { ...; throw e; }` re-throws it, silently killing the reactor worker thread before it ever attempted a TCP connect (confirmed via `strace`: zero `connect()` syscalls before the fix). Fixed by registering `supportedOptions()` on both classes, returning a real `Set` of the options this shim's `apply_option`/`read_option` already recognize (`TCP_NODELAY` genuinely wired, `SO_KEEPALIVE`/`SO_REUSEADDR`/`SO_RCVBUF`/`SO_SNDBUF`/`SO_LINGER` accepted no-ops). Verified real progress: `strace` after the fix shows a genuine `connect()`+`getsockopt(SO_ERROR)=0` (TCP connect now succeeds) where before it never even attempted one; Reactor Netty/Jetty unaffected, `native-io`'s 330 unit tests pass. **Still open**: even after this fix, the request never reaches `MockWebServer` (`requestCount=0`) and the callback never fires -- exhaustively ruled out an uncaught exception on any of 16 `IOReactorWorker` threads (reflectively polled `getThrowable()` on all of them: null, all alive), `BasicFuture`/lock-based callback delivery (bytecode looks sound), and a false-positive connect timeout (`Timeout`/clock arithmetic all correct, 3-minute default). SLF4J TRACE logging (newly enabled via `slf4j-simple` + a `simplelogger.properties`, previously "no providers found") shows HttpClient5's own logging stops cleanly after "connecting ... (3 MINUTES)" with no further lines or errors. The break is somewhere in the connect-completion handoff (`InternalConnectChannel.onIOEvent` в†’ `checkTimeout` в†’ `eventHandlerFactory.createHandler` в†’ `InternalDataChannel.upgrade`/`handleIOEvent`) producing no observable exception, log line, or thread death via any technique tried. Doc has detailed next-step guidance (instrument httpclient5 bytecode directly via ASM, the only remaining lever after reflection/logging/uncaught-handler techniques were exhausted) and lists all 16 reproduction probe files used (not committed, recreate from the doc).

## 2026-07-06 Hibernate `others.txt` non-passed rerun (OSR allocation-region gate branch)

- вњ… FIXED 2026-07-06 (`fix/hib-inpredicate-criteria-values-null-20260706`, commit `084c8ffb`): the `values`-null NPE was `try_osr()` in `vm/src/runtime/interpreter.rs` misreading the invokedynamic uncommon-trap's `i64::MIN` deopt sentinel as a genuine reference return value (no CompactValue/register-staleness involved вЂ” reproduced standalone, no Hibernate needed). Full analysis moved to [`docs/internal/hib-inpredicatetest-criteria-values-null-npe-FIXED.md`](../internal/hib-inpredicatetest-criteria-values-null-npe-FIXED.md); the previously-noted candidate cherry-pick (`4c3cf821` / `fix/hib-inpredicate-criteria-values-null-20260705`) is superseded and unneeded. `InPredicateTest` later exposed the separate `LinkedHashMap.removeEldestEntry` NSME and a stale timeout note; both are now retired after the 2026-07-08 Azure recheck/fix: [LHM NSME](../internal/fixed-suite-bugs/hib-domainparameterxref-lhm-removeeldestentry-nsme-FIXED.md) and [timeout note](../internal/fixed-suite-bugs/hib-inpredicate-dispatch-heavy-jit-timeout-20260707-FIXED.md).
- A 50-class rerun of `others.txt` (4 shards, 1200s per-class timeout) otherwise reconfirmed several already-tracked bugs with no new symptoms: HIB-CV-30 (`MultiLevelCascadeCollectionEmbeddableTest`/`IdClassTest`), the H2/javac `File.pathSeparator` cluster (`SessionDelegatorBaseImplTest` + 4 stored-procedure classes, fix exists on an unmerged branch), `ProxyClassReuseTest`, and `JpaLargeBlobTest`. `SortNaturalTest` showed `HANG` in the parallel sweep but passed cleanly (`ok=1`, 10.5s) in an isolated rerun вЂ” a shared-host contention artifact, not a regression of its 2026-06-22 fix. `DelayedCdiSupportTest` (originally reported alongside this rerun as a genuine hang, see `docs/internal/hib-delayedcdisupporttest-weld-bootstrap-hang-NOT-A-BUG.md`) was later REFUTED as the same contention artifact -- it and all sibling CDI-strategy tests pass cleanly in 2-8s when the host is not under load.

## 2026-07-06 WildFly Host Controller org.jboss.as.jmx module-load NPE

- вњ… FIXED 2026-07-06 (`fix/wildfly-module-descriptor-null-20260706`, commit `657ee914`): `Module.canUse`/`addUses` read `this.descriptor` directly in real bytecode and were missing from `force_native_over_real_jdk_bytecode`, so a registered-but-shadowed native never protected real-JDK mode against a named Module with an unset `descriptor` field. Verified with a standalone repro that fails pre-fix and passes post-fix, plus existing module test suites all passing. Full analysis moved to [`docs/internal/fixed-suite-bugs/wildfly-module-descriptor-null-host-controller.md`](../internal/fixed-suite-bugs/wildfly-module-descriptor-null-host-controller.md); live re-confirmation via `HostExcludesTestCase` end to end remains blocked by the domain-mode MSC real-start gap noted below (orthogonal to this fix, doesn't block closing it).

## 2026-07-06 WildFly domain-mode corrupt-Value-cell root cause + MSC real-start gate

- [wildfly-domain-heap-corrupt-value-timeout.md](wildfly-domain-heap-corrupt-value-timeout.md) вЂ” root-caused: the repeated `gen_heap::read_slot: corrupt Value cell` guard hit during domain-mode boot is the plain-field 16-byte `Value`-slot tearing bug, fixed on `dev` by `2dfdfddc`/`5198fccd` (landed the day after this doc's evidence). Live re-confirmation is blocked by a deeper, separately-tracked gap (below); see the doc for the full analysis and the pre/post-fix code diff.
- [wildfly-domain-managed-servers-timeout.md](wildfly-domain-managed-servers-timeout.md) вЂ” updated 2026-07-06: hand-driving `standalone.sh` under `CRATONVM_MSC_REAL_START=1` used to hit a `ServiceNotFoundException` ~3s into boot; **now FIXED** вЂ” see [bug-15](../internal/fixed-suite-bugs/wildfly/bug-15-msc-real-start-servicenotfound-and-domain-hang.md), which also fixed a worker-pool race that was silently faking service starts (masking whether `Service.start()` ever really ran). Fixing both exposed two new, deeper blockers in the same boot path: a `ServiceController.addListener` `AbstractMethodError` (no native hook backs the real interface method), and an as-yet-undecoded real exception now thrown by `ApplicationServerService.start()` itself once it actually runs. Domain mode (not just standalone) still hangs separately under the same flag, untouched by this fix.


## 2026-07-06 vm crate unit-test residuals (branch fix/vm-monitor-test-object-heap-uaf)

- [vm crate unit-test residuals post-monitor-fix](vm-unit-test-residuals-post-monitor-fix.md) - after fixing a test-helper use-after-free SIGSEGV that was crashing `cargo test -p cratonvm-vm --release --lib` before it could finish, the suite surfaced 16-17 masked failures. 8 were a false alarm (lock_order enforcement gated behind debug_assertions, disabled under --release), 3 were stale hardcoded bootstrap-class/native counts (FIXED here вЂ” a legitimate recent feature grew the count from 5 to 25 classes), 1 didn't reproduce in debug (not investigated). 4 remain open: virtual_scheduler over-release accounting (design question), vm_exec object-pointer-provenance test predates a security hardening, runtime::frame CompactValue long/upper-half slot-tearing-adjacent bug, and a jit::skip_list Keycloak over-match not yet traced to its exact matching branch.

## 2026-07-05 Hibernate pruned residuals

- Hibernate JpaLargeBlobTest Object.read() dispatch вЂ” now **FIXED** (2026-07-06), moved to [`docs/internal/fixed-suite-bugs/hib-jpalargeblobtest-object-read-nosuchmethod.md`](../internal/fixed-suite-bugs/hib-jpalargeblobtest-object-read-nosuchmethod.md). Two distinct bugs: (1) JIT virtual/interface MIC helper resolving `ClassId(0)` non-Object receivers to `java/lang/Object` (merged `b09fea46`), and (2) a residual GC-staleness bug вЂ” `native_bais_read_bytes`/`native_dis_read_bytes` in `native-io/src/lib.rs` held unpinned `ObjectRef` locals across re-entrant `invoke_virtual` calls, so a moving GC mid-loop could strand them (fixed with `pin_native_root`/`read_native_pin`).

## 2026-07-04 test.context.* cluster (bean/groovy/junit/junit4/testng/web, branch fix/test-context-cluster)

- [Constructor-parameter-annotation offset fix + 3 residuals](test-context-constructor-param-annotation-offset.md) вЂ” core fix: `Constructor.getParameterAnnotations()`'s native override didn't account for a synthetic leading parameter (non-static inner-class constructors' implicit outer-instance arg), causing an AIOOBE that crashed 13 of 25 CV-unique classes across the six packages (7 directly + cascading ABEND in 6 more sharing a batch). 3 residuals open: Groovy TestContext script loading (isPresent stub blocks it; removing the stub exposes a separate ANTLR/jarjar class-layout bug вЂ” not fixed), `InheritableThreadLocal` not propagated through `Thread(ThreadGroup, Runnable, String[, long])` constructors (breaks `Executors`-backed pools generally, not just these tests), and one JUnit5-parallel-execution TIMEOUT not yet triaged.


## 2026-07-04 -> 2026-07-06 http.server bug cluster (RETIRED, split up)

Iterated across several sessions (branches `fix/http-server-cluster`,
`fix/httpserver-pkcs12-20260706`, `fix/httpserver-certfactory-20260706`,
`fix/httpserver-zerocopy-bytebuddy-race-0706b`). 10 of 12 target classes
fully fixed (`Collections.emptyListIterator()` mis-stamp crashing Jetty's
main thread; `newSetFromMap(LinkedCaseInsensitiveMap)` losing
case-insensitivity; `java.net.URI`/`URLDecoder`/`URLEncoder` gaps;
`Provider.putService` + per-instance `containsKey`; `CertificateFactory`
real-SPI delegation; PKCS12/PBE empty-password guard; real-JDK-mode TLS
native reachability). `ZeroCopyIntegrationTests`'s flakiness is now
**FIXED** (an ABBA `class_manager`/`vtable_manager` lock-order deadlock,
the same bug independently found in the http.client cluster below). Full
fix writeup moved to
[`docs/internal/fixed-suite-bugs/http-server-cluster-residuals-fixes-FIXED.md`](../internal/fixed-suite-bugs/http-server-cluster-residuals-fixes-FIXED.md).
Residual tracking:

- вњ… FIXED (branch `fix/tls-identity-singleton-clobber-20260707`, 2026-07-07):
  `http-server-sslengine-identity-singleton-clobber` вЂ” the identity-singleton
  clobber and its "malformed key" root cause. FOUR distinct CratonVM bugs found
  and fixed: RSA `KeyPairGenerator` producing non-CRT keys with a 572-byte
  `getEncoded()`, the `RUNTIME_TLS_IDENTITY` clobber (validate-before-overwrite),
  `CertificateFactory.generateCertificate` empty `getEncoded()` on PEM streams,
  and `generateCertificate` only reading a `ByteArrayInputStream`'s field-0
  buffer. Doc retired to
  [`../internal/http-server-sslengine-identity-singleton-clobber-FIXED.md`](../internal/http-server-sslengine-identity-singleton-clobber-FIXED.md).
  `ServerHttpsRequestIntegrationTests` now reaches TLS record processing (server
  config builds) and initially failed on a separate SSLEngine handshake
  data-flow bug вЂ” вњ… FIXED (branch `fix/netty-sslengine-underflow-20260707`,
  2026-07-07): a `bb_view` gap for `DirectByteBuffer`-backed engines, plus
  four further chained bugs (`SSLEngineResult` accessor natives clobbering
  the REAL enum singletons, client `TrustManager` delegation, client
  `SSLSession` peer-chain population, and synthetic `SSLSocketOutputStream`/
  `InputStream` missing their real superclass). Doc retired to
  [`../internal/reactive-netty-https-sslengine-handshake-underflow-FIXED.md`](../internal/reactive-netty-https-sslengine-handshake-underflow-FIXED.md).
  That fix chain exposed one more residual вЂ” the client's actual POST write
  failing with `SSLSocketOutputStream.write: stream is closed` вЂ” which is
  itself now вњ… FIXED (branch `fix/netty-socket-write-after-close-20260708`,
  2026-07-08): EIGHT further chained bugs, the core one being
  `alloc_concurrent_synthetic` sizing a synthetic `SSLSocket` using the
  REAL loaded class's field layout, so a raw `Int` field write to the
  connection-id slot was silently dropped by the GC/field-layout guard
  (fixed by migrating that state into `net_phase_e`'s existing `SockSide`
  side table); the final flaky residual was `isInputShutdown`/
  `isOutputShutdown` never having a reachable native registration, so real
  bytecode read garbage from the synthetic object and non-deterministically
  told Apache HttpClient5 the connection was already closed. Doc retired to
  [`../internal/netty-client-socket-write-after-close-nsme-FIXED.md`](../internal/netty-client-socket-write-after-close-nsme-FIXED.md).

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
- [SmallRye Config missing Charset/MemorySize converters](../internal/fixed-suite-bugs/keycloak-smallrye-config-charset-memorysize-converters.md) - `LoggingSetupRecorder.handleFailedStart()` now builds its transient logging config with discovered Quarkus converters. The local 2026-07-04 deep dive also showed this fix exposes a later test-framework/Maven-artifact resolution gap rather than unlocking all `tests/base` classes outright. That gap is also now fixed: [test-framework deployRequestedInstances resolution failure](../internal/fixed-suite-bugs/keycloak-testframework-linkedlist-addall-deployrequestedinstances-FIXED.md) (root cause: `LinkedList.addAll(LinkedList)` silently dropped elements). The Phaser/ForkJoinPool hang this exposed next is also now fixed: see the 2026-07-07 entry near the top of this file.
- [quarkus/runtime CompactValue NaN-box collision SIGSEGV](../internal/fixed-suite-bugs/keycloak-quarkus-compactvalue-nanbox-sigsegv.md) - current `dev` no longer reproduces `rc=139`. The post-crash PicocliTest timeout residual is also fixed in [PicocliTest post-CompactValue-fix hang](../internal/fixed-suite-bugs/quarkus-runtime-picocli-post-compactvalue-hang.md).
- [System Rules getenv() field 'm' reflection mismatch](../internal/fixed-suite-bugs/keycloak-system-rules-getenv-field-m-reflection.md) - `System.getenv()` now exposes an OpenJDK-shaped unmodifiable map wrapper whose private `m` field points at the backing map.
- [KcAdmV2HelpTest --help text env-var mentions](../internal/fixed-suite-bugs/keycloak/kcadmv2-helptext-env-var-mentions.md) - synthetic `BreakIterator.getLineInstance()` now returns Java UTF-16 text offsets after complete whitespace/hyphen runs, so Picocli no longer hard-wraps `KC_CLI_*` env-var names.
- [IgnoredArtifactsTest.multipleDatasources datasource properties missing](../internal/fixed-suite-bugs/quarkus-runtime-ignoredartifacts-multipledatasources-boolean.md) - module-scoped runner working directories now match Keycloak's relative `src/test/resources` setup, and Windows `File.toPath().toUri()` no longer emits `%5C` backslash URIs.
- [LoggingConfigurationTest getPropertyNames stale log-category key](../internal/fixed-suite-bugs/quarkus-runtime-logging-getpropertynames-garbage-key.md) - `System.setProperties(Properties)` now replaces the VM-wide properties state, so stale fixture keys do not leak into SmallRye property-name iteration.
- [LoggingConfigurationTest wildcard DEBUG level resolves null](../internal/fixed-suite-bugs/quarkus-runtime-logging-wildcard-debug-level-null.md) - fixed by the same System-properties replacement semantics as the stale log-category key.
- [TelemetryConfigurationTest telemetry-service-name wrong value](../internal/fixed-suite-bugs/quarkus-runtime-telemetry-service-name-wrong-value.md) - fixed by System-properties replacement/reset semantics.

Already-tracked, not re-documented: the 37 `testsuite/model` CRASHes were the
[Infinispan GlobalConfigurationBuilder.isClustered() NoSuchMethodError](../internal/fixed-suite-bugs/keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod-FIXED.md),
now **FIXED** (2026-07-06) вЂ” moved to `docs/internal/fixed-suite-bugs/`. The
same identity-wrapper bug shape one step deeper in the same boot path,
[Infinispan ConfigurationBuilder.build() ClassCastException](../internal/fixed-suite-bugs/keycloak-model-infinispan-configurationbuilder-classcastexception.md),
is now **also FIXED** (2026-07-06). `RealmModelTest` then reached a residual
that was initially misdiagnosed as a Netty `PlatformDependent0` setAccessible
bug -- that diagnosis was wrong (refuted via bytecode decompilation + A/B
testing against real JDK 25); the actual cause,
[Infinispan JGroupsTransport.start() never invoked](../internal/fixed-suite-bugs/keycloak-model-jgroupstransport-start-never-invoked-FIXED.md)
(a `DefaultCacheManager.start()`/`stop()` native shim intercepting real
objects unconditionally), is now **also FIXED** (2026-07-06). `RealmModelTest`
then reached a distinct residual one layer deeper, in the same "synthetic
native shim intercepts a real object" family,
[Infinispan Cache.config null after real DefaultCacheManager.start()](../internal/fixed-suite-bugs/keycloak-model-infinispan-cache-config-null-after-real-start-FIXED.md),
now **also FIXED** (2026-07-06); the historical JIT-only decode-error note is
retired, the current open follow-up is the `RealmModelTest` Liquibase-phase
timeout, and the sibling `--nojit` STW shutdown hang is fixed and retired to
[`docs/internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`](../internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md).
The former Arquillian `auth-server-undertow` container-provisioning gap is
fixed in the per-class runner: it now forwards the effective Maven Surefire
bootstrap configuration. See the [retired record](../internal/fixed-suite-bugs/keycloak-arquillian-auth-server-undertow-container-not-found-FIXED.md).
25 additional CRASHes (`scim/core`, `ssf/*`,
`test-framework/*`, `tests/webauthn`, 2x`tests/clustering`) were a harness
classpath gap (missing `junit:junit`, fixed alongside the JUnit Platform
launcher fix above), not a VM bug.

## 2026-07-03 -> 2026-07-06 http.client bug cluster (RETIRED, split up)

Iterated across several sessions (branches `fix/http-client-cluster-azure`,
`fix/httpclient-residuals-20260706`,
`fix/httpclient-vtable-classmanager-abba-deadlock-0706c`,
`fix/httpclient-bytebuddy-mh-dispatch-0706b`,
`fix/httpclient-jdkclient-hangs-0706b`). Core fix: two of three "native
shadow" dispatch paths never checked whether an ANCESTOR class (not just
the receiver) was JVMTI-redefined, so Mockito-mocked concrete classes
(e.g. `HttpURLConnection`) silently bypassed their own advice. Plus
several JDK 21 real-mode native gaps, a `HashMap`/`HashSet` chain-walk
guard gap, and an ABBA `class_manager`/`vtable_manager` lock-order
deadlock (cut `HttpComponentsClientHttpRequestFactoryTests`'s teardown
hang rate from 65% to 25%). Full fix writeup moved to
[`docs/internal/fixed-suite-bugs/http-client-cluster-redefine-dispatch-fixes-FIXED.md`](../internal/fixed-suite-bugs/http-client-cluster-redefine-dispatch-fixes-FIXED.md).
Residuals split into their own focused docs:

- ~~class-manager-rwlock-writer-starvation.md~~ вЂ” the residual 25% hang rate above: FIXED 2026-07-07 (was a same-thread recursive-read deadlock, not writer starvation вЂ” see `docs/internal/fixed-suite-bugs/class-manager-rwlock-recursive-read-deadlock-FIXED.md`).
- [http-client-simpleclienthttpresponsetests-mockito-dispatch-bugs.md](http-client-simpleclienthttpresponsetests-mockito-dispatch-bugs.md) вЂ” `SimpleClientHttpResponseTests`'s `UnfinishedVerificationException` (order-dependent) + an intermittent `(class, method, descriptor)`-substituting `NoSuchMethodError`; several hypotheses refuted, root cause still open.
- [spring-web-flow-outputstreamwriter-close-corruption.md](spring-web-flow-outputstreamwriter-close-corruption.md) вЂ” 2 of 4 "genuine hang" classes root-caused to `StreamEncoder.closed` being `true` immediately from `OutputStreamWriter` construction (mechanism itself still unexplained); other 2 need separate investigation.
- ~~http-client-reactor-windows-timeout-linux-epoll-gap.md~~ вЂ” RETIRED 2026-07-08: current `dev` passes `ReactorClientHttpRequestFactoryTests` 10/10 on the Azure real-JDK Spring probe; archived at [`docs/internal/fixed-suite-bugs/http-client-reactor-windows-timeout-linux-epoll-gap-FIXED.md`](../internal/fixed-suite-bugs/http-client-reactor-windows-timeout-linux-epoll-gap-FIXED.md).

`SimpleClientHttpRequestFactoryTests`'s residual failures are pre-existing,
out-of-scope synthetic-`HttpURLConnection` gaps unrelated to this cluster
(memory `huc-setdooutput-wrong-slot-drops-post-body`) вЂ” not re-documented.

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
  falling back to the "not implemented" IOException вЂ” covers `jmxmp`
  end-to-end with real provider bytecode instead of a canned error.
- FIXED (branch fix/jmx-platform-mxbean-registration): platform MXBeans
  (Memory, Threading, etc.) were never actually registered onto the real
  MBeanServer returned by `ManagementFactory.getPlatformMBeanServer()` вЂ”
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
  вЂ” 2 of the original 4 test methods still fail: `mxBeanOperationAccess()`
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
  Its remaining 1/18 DoHead crash face was not an XT activation residual; that
  DoHead crash family's FATAL layer is now itself fixed too (2026-07-06,
  `fix/dohead-sweep-freelist`) вЂ” doc moved to
  [`docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`](../internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md).
  Its one real residual (Layer 1, register-invisible roots) is now retired by
  the Fork6/A4 fix; see
  [`fork6-fjp-multithread-jit-root-reclamation-FIXED.md`](../internal/fixed-suite-bugs/fork6-fjp-multithread-jit-root-reclamation-FIXED.md).
- FIXED/RETIRED (2026-07-13): `abstractajpprocessor-socket-not-connected.md`
  (AJP, `TestAbstractAjpProcessor` 30/30 fail) shared its exact root cause
  with the HTTP/2 `Socket is closed` cluster fixed the same day вЂ”
  `SimpleAjpClient.connect()` goes through the same
  `javax/net/SocketFactory.getDefault().createSocket(...)` call as
  `Http2TestBase`. Doc moved to
  [`abstractajpprocessor-socket-not-connected-FIXED.md`](../internal/fixed-suite-bugs/tomcat/abstractajpprocessor-socket-not-connected-FIXED.md).
  The AJP secret residual is now fixed and retired; see
  [`ajp-testsecret-secret-attribute-not-enforced-FIXED.md`](../internal/fixed-suite-bugs/tomcat/ajp-testsecret-secret-attribute-not-enforced-FIXED.md).
  The AJP no-headers residual is also fixed and retired; see
  [`ajp-testnoheaders-response-body-not-empty-FIXED.md`](../internal/fixed-suite-bugs/tomcat/ajp-testnoheaders-response-body-not-empty-FIXED.md).

## How many distinct bugs are here?

After consolidation (full re-count 2026-06-18, kafka-bug-B/C status reconciled 2026-07-01),
the ~30 docs map to **one root-cause family + ~16 distinct standalone bugs**, of which
**12 are already FIXED on `dev`** (the prior 11 plus the AJP secret-rejection fix). Headline:

**~6 distinct OPEN defects + 1 latent** (was ~7 after the AJP secret-rejection fix), grouped as:

1. **Family A - GC root coverage under JIT** (one root cause, several manifestations). Open members:
   none from the Fork6/FJP lane; **A1/A2/A3/A4/A5 are FIXED**. **A2**
   (`ReflRepro`) was re-diagnosed and fixed 2026-06-23 (`6e3ddb05`): it was **never** a
   register/native-return missed root, but a GC-side non-moving-sweep free-list double-serve
   (overlapping free blocks not coalesced в†’ `Arena::alloc` served the same region twice); A5 was
   root-caused to the **compiled entry-point
   `main`'s JIT frame being unregistered** (invoked via `Vm::invoke` without a `JitEntryGuard`, so
   the moving young collector relocated its roots); fixed by detecting an unregistered JIT frame on
   the native stack в†’ non-moving sweep + full-stack mark (dev `77c98761`; writeup moved to
   [`docs/internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md`](../internal/fixed-suite-bugs/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md)).
   `spring-bug-10` is a Family-A manifestation seen from
   the Spring suite (same root cause, different entry point); `springsuite-bug-04` was the same race
   but **no longer reproduces** (doc removed). The suite-scale field evidence in
   `jit-junit-discovery-reflection-corruption.md` is the same race.
   **Parked-thread deposit member FIXED 2026-07-02**: the blocked-path
   `deposit_root_snapshot` served a stale `JIT_SCAN_CACHE` snapshot (missing
   `invalidate_scan_cache_for_gc()`), so a JIT'd `AQS.await` frame's freshly
   inline-allocated `ConditionNode` was never pinned and selective promotion
   moved it under the parked thread вЂ” the 100%-reproducible
   `ConcurrencyThrottleInterceptorTests` TIMEOUT wedge. Writeup:
   [`docs/internal/fixed-suite-bugs/throttle-park-deposit-stale-jit-scan-cache.md`](../internal/fixed-suite-bugs/throttle-park-deposit-stale-jit-scan-cache.md).
2. **Standalone B** вЂ” JUnit `@Timeout` interceptor double-`proceed()` (open).
3. **Standalone C** вЂ” deep JITв†’JIT recursion native-stack overflow (latent; stack-bang containment landed; resumable fault recovery still open).
4. ~~**bug-06 F5**~~ вЂ” reflection native returns null vs a `Class`/`Method`: **вњ… CLOSED 2026-07-02,
   failcause extinct** вЂ” 0 instances in the clean 2026-06-30/07-01 full re-runs and in a fresh
   196-class nojit+jit sweep on dev `ffb247e5`; it was a cross-family cascade whose sources
   (fam1/3/4, `toArray` recursion, bug-04 GC, bug-05 generics) are all fixed (doc moved to
   [`docs/internal/fixed-suite-bugs/bug06-fam5-reflection-getdeclaredmethod-null.md`](../internal/fixed-suite-bugs/bug06-fam5-reflection-getdeclaredmethod-null.md)).
5. **bug-06 F6 / `spring-bug-06` / `spring-bug-01`** вЂ” annotation **synthesis** value-mismatches.
   (The `MergedAnnotations` **hang** + the `AnnotationUtilsTests` "~2 GB OOM" in this cluster were the
   `toArray` self-recursion and are **FIXED** 2026-06-20, commit `8795b88d`; `MergedAnnotationsTests`
   now runs 174/178, `AnnotationUtilsTests` 72/72. Only the synthesis value-mismatches remain open.)
6. **`spring-bug-08`** вЂ” serializable JDK-proxy round-trip (open).
7. **`spring-bug-11` residual** вЂ” Groovy hang at `BEGIN` (the SIGSEGV half is FIXED via bug-12; open).
8. ~~**`kafka-bug-B`**~~ вЂ” Mockito `mockStatic` + `mock`/`mockConstruction` dispatch:
   **FIXED on `dev`**. The stale JIT call-site residual was fixed by redef-time compiled
   dispatch quiescing; archived at
   [`docs/internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md`](../internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md).
9. ~~**`kafka-bug-C`**~~ вЂ” `WeakHashMap.values().stream()` infinite hang: **FIXED on `dev`** (`1cd0ab26`; doc removed).
10. **Hibernate JTA** (Narayana) вЂ” вњ… **RESOLVED 2026-06-20** (docs в†’ [`docs/internal/`](../internal/)).
    L0 fixed on dev; **L1 was never broken (refuted)**; **L2 = an `accept()` deadlock** (global `s2_registry`
    lock held across blocking `accept()`) + `getLocalPort()==0`, both fixed on branch `fix/hib-jta-xa-loopback`
    (`e0426050`). A full Hibernate 8.1 + Narayana 7.3.4 + H2 begin/persist/commit/truncate cycle now passes in
    default (synthetic-socket) mode.
11. **Hibernate JAXB/ByteBuddy bootstrap slow** вЂ” вњ… **RESOLVED / does-not-reproduce 2026-06-20**
    (doc в†’ [`docs/internal/`](../internal/)). Class-load storm fixed on dev; ByteBuddy `MethodGraph`
    JoinedSubclass bootstrap completes ~16s `--nojit`.
12. **Hibernate deserialized-`SessionFactory`-null** (`SessionFactoryRegistry` reconnect; open).
13. **ES-HANG-02 вЂ” вњ… RESOLVED 2026-06-20** (`fix/es-restclient-gc-safety`; docs moved to
    [`docs/internal/elasticsearch-suite/`](../internal/fixed-suite-bugs/elasticsearch-suite/)). The RestClient
    embedded-HTTP-server hang AND both residuals are fixed: residual 1 (real non-blocking connect) +
    residual 2 вЂ” which was **NOT throughput** (the prior handoff's theory) but **three GC-correctness bugs**:
    a safepoint-resume monitor/native-root remap gap (`IllegalMonitorStateException`), an unrooted synthetic
    `com.sun.net.httpserver` handler ref (`NoSuchMethodError java/lang/Object.handle` storm), and
    per-request native read-alloc-use staleness. Both ES RestClient suites are green at `-Xmx1g` with
    `CRATONVM_ROOTSNAP_CACHE=0` (single-host 22/22, multi-host 4/4) вЂ” see the two new GC defects #15/#16.
    Former siblings also resolved: **ES-HANG-01** (`1cd0ab26`), **ES-FAIL-03**, **ES-FAIL-04**.
14. **Spring-suite sweep (2026-06-19 в†’ re-verified 2026-06-20).** Status after running the suite
    through the JUnit-Platform `KRun` harness on a fresh dev build (`cratonvm-spring0620`, off
    `697134f8`):
    - вњ… **toArray-recursion FIXED** (commit `8795b88d`, в†’ dev) вЂ” `ReferencePipeline.toArray(IntFunction)`
      was shadowed by a native that re-entered no-arg `toArray()` в†’ `StackOverflowError`. This was the
      open **`MergedAnnotations` hang** AND **bug-06 fam6 "~2 GB OOM"**, and it blocked the *entire*
      JUnit launcher (no test class could run). Now: `MergedAnnotationsTests` 174/178,
      `AnnotationUtilsTests` 72/72, `AnnotatedElementUtilsTests` 82/82. Doc:
      [`docs/internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md`](../internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md).
    - вњ… **Unsafe off-heap DirectBuffer (bug-A + bug-A2) FULLY RESOLVED** вЂ” `PooledDataBufferTests`
      **10/10**, `LeakAwareDataBufferFactoryTests` 2/2. The bug-A2 Netty `refCnt` AIOOBE no longer
      reproduces. Archived в†’ [`docs/internal/fixed-suite-bugs/springsuite-0619-unsafe-offheap-directbuffer.md`](../internal/fixed-suite-bugs/springsuite-0619-unsafe-offheap-directbuffer.md).
    - вњ… **getBeanClassName bean-filter (bug-B) FIXED** вЂ” primary filter fix holds, and **bug-B2
      (CGLIB method-injection) is fully implemented 2026-06-21** (commit `ffa71253`): the instantiate
      shim synthesises a concrete subclass overriding each abstract `<lookup-method>`/`@Lookup` method
      via `bf.getBean(name|args)` (NullBeanв†’null), `bf.getBeanProvider(ResolvableType.forMethodReturnType(m)).getObject()`
      for generic by-type, with overload-aware + child-precedence override matching.
      **`LookupMethodTests` 0/7 в†’ 7/7** (JIT and `--nojit`), **`LookupAnnotationTests` 0/10 в†’ 10/10**.
      (The "flaky JIT `invokeVoid`" turned out to be a one-byte emitter typo вЂ” `IFEQ` vs `IFNULL` вЂ”
      not a VM defect.) [bug-B / bug-B2 doc](../internal/fixed-suite-bugs/spring/springsuite-0619-getbeanclassname-bean-filter.md) вЂ” вњ… **FIXED on dev** (`6d596473`; moved to docs/internal).
    - Still untriaged from the sweep: `ReactiveAdapterRegistry$MutinyRegistrar` NCDFE, XML
      "Unexpected failure during bean definition parsing", "Unnamed bean definition", spring-jdbc
      mass-TIMEOUT, scheduler `StringIndexOutOfBounds`, and `DataBufferUtilsTests` TIMEOUT
      (heavy-reactive). (The prior `springsuite-0619-open-candidates.md` link was already dangling.)
15. **[GC: moving-collector lost-tag missed root](../internal/gc-moving-interpreter-lost-tag-missed-root.md)** вЂ”
    вњ… **FIXED on dev** (`0abb64ba`, 2026-07-01; doc already archived under `docs/internal/`). The **moving**
    young collector zeroed a live object whose only reference was a frame slot tagged non-`Object` at the
    marking snapshot. Fixed in `Frame::scan_local_objects`/`update_local_refs`: non-object-tagged locals are
    probed via `lost_tag_local_candidates` + the strict `is_object_address` header check, then rooted and
    remapped. Unit-tested (`scan_local_objects_roots_lost_tag_other_local`,
    `update_local_refs_remaps_lost_tag_other_local`); the `CRATONVM_GC_VERIFY_STALE` verifier is retained.
16. **[GC: rs_cache-presence reactor-shutdown timing race](../internal/gc-rscache-reactor-shutdown-timing-race.md)** вЂ”
    вњ… **FIXED on dev** (`323a3ba6`, 2026-07-01: thread exit serialized against STW вЂ”
    `request_stw_counted` computes `alive_count` under the barrier lock, blockedв†’dead transition is atomic,
    thread teardown uses a stable inflated `Arc<Monitor>`; unit-tested; doc archived to `docs/internal/`
    2026-07-07). Residual validation item recorded in the archived doc: the ES
    `RestClientSingleHostIntegTests` `-Xmx1g` default-cache soak; the `CRATONVM_ROOTSNAP_CACHE=0`
    workaround is expected unnecessary. **NOT** a socket/OP_WRITE bug and **NOT** an rs_cache correctness bug.
17. **[CompletableFuture untimed `get()` never wakes on cross-thread completion](../internal/fixed-suite-bugs/app-jvm-bugs/gc-gen-promotion-completablefuture-completion-loss.md)** вЂ”
    вњ… **FIXED on dev** (moved to `docs/internal/app-jvm-bugs/`). The original gen-GC "lost young `Signaller`"
    theory was **refuted** (the hang is deterministic + GC-independent); the real cause was the synthetic
    `CompletableFuture.complete` native never running `postComplete()`. Fixed in the native вЂ” no GC change.
18. **[GC: live young `Thread` mirror in a blocked thread's frame reclaimed (Tomcat real-net/real-AQS HARD CRASHES)](gc-blocked-thread-frame-stale-thread-mirror.md)** вЂ”
    рџџ  **OPEN on dev**, but a **defensive crash-mitigation is MERGED** (`6a04b0e3` + `e06ed934`). Six Tomcat
    encoding/tribes classes SIGSEGV/panic (`compact_value.rs:502`) under `CRATONVM_REAL_NET_SOCKETS` +
    `CRATONVM_REAL_AQS`: a live young `java.lang.Thread` mirror held in a **blocked** thread's frame local
    (object-tagged, so NOT the lost-tag item 15) is reclaimed because it is missing from that thread's
    deposited `root_snapshot`; the freed slot is reused as byte-buffer data and decoded as an object pointer.
    Related to the now-fixed `currentThread()` mirror construction corruption archived at
    [`../internal/repros/gc-concurrent-spawn-reclamation/`](../internal/repros/gc-concurrent-spawn-reclamation/).
    Current `dev` has that re-read / pin fix and the stale-mirror recovery table, but this Tomcat item remains
    open for the blocked-frame snapshot gap and register-resident remainder. The mitigation (`plausible_heap_pointer`
    gate at every ref-decode + JIT receiver-deref boundary) degrades a stale ref to a Java NPE вЂ” all 6 are now
    **crash-free jit+nojit** but still fail/time out (the residual reclamation itself is unfixed).
19. **[Hibernate `type.temporal.*` вЂ” moving GC strands lambda refs in native stream/collection intrinsics](../internal/hib-temporal-gc-lambda-native-stale-local.md)** вЂ”
    вњ… **FIXED on dev** (`3240cb75`, 2026-07-06; doc archived to `docs/internal/` 2026-07-07). The
    native-stale-Rust-local family this doc identified was closed at three levels: the earlier
    native-collections + scheduled-pump pin sweep, a 242-site `pin_native_root`/`read_native_pin` sweep of
    `phases_late.rs`, and вЂ” the keystone вЂ” `invoke_shared`/`invoke_special_shared` now pin object args
    across the class-load + `<clinit>` window (`5732a1e1`), which was the residual stale-args hole no
    per-native pin could cover. Deterministic repro
    (`IntStream.concat(s.chars(), s.chars())` at `CRATONVM_DBG_GC_STRESS=32768`, failed <1s) is green
    Г—5 + full-length, jit and nojit, checksums == HotSpot. Remaining validation noted in the archived
    doc: the 5 `org.hibernate.orm.test.type.temporal.*` classes loop at default heap.

FIXED bugs whose standalone docs were **removed** from this folder (resolved; full writeups in
`git` history or [`docs/internal/fixed-suite-bugs/`](../internal/fixed-suite-bugs/)): A1 (reflection
mirror-array pinning), A3 (register-invisibility вЂ” precise maps default-on), the Hibernate JAXB
class-load rescan storm (HIB-DEV-03), the JSON-function `al_state` SIGSEGV, the reversed stack-trace
order, the `ReferencePipeline.toArray(IntFunction)` recursion, and the off-heap DirectBuffer
(bug-A/A2). The springrepos hang is mostly fixed (`dev` passes the test; only the latent
deep-recursion item remains вЂ” see below).

### Consolidations applied (2026-06-18)
- The two Hibernate-JTA docs (`hibernate-jta-narayana-вЂ¦` + `hibernate-jta-txcontrol-getinetaddress-per-class-report`)
  described the **same** Narayana cluster в†’ merged into `hibernate-jta-narayana-xa-completion-and-socket-loopback.md`;
  the per-class file is now a redirect stub.
- (2026-06-17) The two Fork6 precise-maps files were merged into `fork6-fjp-multithread-jit-root-reclamation.md`; the combined A4 note is now retired as [`fork6-fjp-multithread-jit-root-reclamation-FIXED.md`](../internal/fixed-suite-bugs/fork6-fjp-multithread-jit-root-reclamation-FIXED.md).

The former **JIT regalloc callee-saved-register clobber** umbrella family is
resolved on x64 dev (2026-07-04) by making callee-saved GPR local homes opt-in only;
the archived write-up is
[`docs/internal/jit-regalloc-callee-saved-clobber-family.md`](../internal/jit-regalloc-callee-saved-clobber-family.md).
Do not conflate that historical register-*clobber* family with Family A below,
which is about root-*scanning* completeness.

---

## Family A вЂ” GC root coverage under JIT  *(one root cause, four manifestations)*

**Root cause (shared):** when a JIT frame is active, the young collection is the
**non-moving sweep with selective-promotion evacuation** (`gc_quiescence`; the
moving Cheney collector only runs with no JIT frame). That sweep is only correct
if the **root set is complete**. CratonVM's JIT-frame root scan is *conservative*
(it reads stack memory only) and has historically had gaps; a live young object
whose only reference sits in a gap is not marked (and not added to the
selective-promotion pin set) в†’ it is swept-zeroed or evacuated-and-zeroed в†’ its
stale reference later reads an all-zero / garbage header в†’ `inconsistent header`,
`Stale pointer вЂ¦ all-zero header`, CCE, NPE, or SIGSEGV. `--nojit` always passes
(interpreter frames are precisely scanned and the moving collector remaps every
root); `-Xmx8g` passes (no young GC).

> **Current-dev refresh (re-verified 2026-06-29, fresh build off dev HEAD
> `9928052c`; precise-jit-maps-default Steps 1вЂ“8).** **A3 is CLOSED** by precise
> JIT oop maps (default-on) вЂ” validated green across the GC-root repro+bench lane,
> OSR frames, and the BouncyCastle app suites
> (`test-infra/regression-pool/gc-root-lane.sh` + `gc-root-apps-lane.sh`).
> **A2 is now FIXED** (`6e3ddb05`, 2026-06-23): `ReflRepro 8000 @ GC_STRESS=65536`
> в†’ `ok=8000 bad=0 rc=0` (and `20000 @ GC_STRESS=524288 --Xmx 256m` в†’ `bad=0`),
> previously `rc=139`. It was **never** a register/native-return missed root вЂ” it
> was a GC-side non-moving-sweep free-list double-serve (overlapping free blocks
> not coalesced), fixed in the sweep coalescer; precise maps are orthogonal.
> **A4 is FIXED (2026-07-09):** the gated real-FJP
> `CRATONVM_REAL_FORKJOINPOOL=1` repro is retired. The final A4 fix combines
> live-blocked STW accounting, correct `Unsafe.compareAndExchange*` witness
> returns, and all-live reference-local scans on the real-FJP non-moving root
> snapshot path. Final validation: plain `Fork6` and `Fork6Hard 128 20` are
> `ALL-OK`, and aggressive `Fork6Hard 128 20` with `GC_STRESS=65536` passed
> 48/48 (`fail=0 timeout=0 signal_logs=0`) on the unique Linux binary
> `cratonvm-fork6-alllive-caxwitness-20260709-012308`. The earlier
> concurrent-old-gen `GC_STRESS` bug and the later real-FJP residual are also
> fixed and archived at
> [gcstress-concurrent-oldgen-races-FIXED.md](../internal/gcstress-concurrent-oldgen-races-FIXED.md)
> and [gcstress-residual-corruption-faces-FIXED.md](../internal/gcstress-residual-corruption-faces-FIXED.md).
> The repros and `CRATONVM_DBG_*` knobs are retained as regression tools.

The register-only portions of this family are covered by **precise JIT stack roots**
(know exactly which registers/slots hold oops at each safepoint), tracked under
`project_precise_jit_stack_maps`. The retired Fork6/FJP A4 path needed separate
real-FJP root/CAS/local-snapshot fixes. The `CRATONVM_SHADOW_STACK` mechanism is
retained as experimental scaffolding.

| # | Manifestation | Repro | Status | Doc |
|---|---|---|---|---|
| **A1** | Reflection mirror-array builders held an `ObjectRef` array in a Rust local across allocating calls (`Field[]`/`Method[]`/annotation arrays) | `wildfly-suite/repro/MinRepro` | вњ… **FIXED on dev** (`pin_native_root` sweep) | _(doc removed; resolved)_ |
| **A2** | **`implausible object size` young-sweep-walker crash** (reflection/String-array allocation churn) вЂ” a *distinct* bug, NOT the register root: a non-moving-sweep free-list double-serve (overlapping free blocks not coalesced) | [`../internal/repros/A2-reflrepro/`](../internal/repros/A2-reflrepro/) | вњ… **FIXED on dev** (`6e3ddb05`, 2026-06-23; coalesce overlapping free blocks) вЂ” `ReflRepro 8000 @ GC_STRESS=65536` в†’ `ok=8000 bad=0` (re-verified 2026-06-29); precise maps orthogonal; moved to docs/internal | [reflrepro-register-resident-jit-root-handoff.md](../internal/fixed-suite-bugs/app-jvm-bugs/reflrepro-register-resident-jit-root-handoff.md) |
| **A3** | **Register-invisibility** вЂ” a live oop sits only in a CPU register at a young-GC safepoint, invisible to the stack-only scan (single thread) | `apps/spring-boot/buildSrc/runner/MinRegexProbe` | вњ… **FIXED on dev** (`32649b56`, precise maps default-on) | _(doc removed; resolved)_ |
| **A4** | Real-FJP multi-thread stale task/root reclamation under `Fork6` / `Fork6Hard` | `scratch/xworker/Fork6` / `docs/known-issues/repros/A4-fork6/Fork6Hard.java` (needs `CRATONVM_REAL_FORKJOINPOOL=1`) | FIXED on dev (2026-07-09) - live-blocked STW accounting + `Unsafe.compareAndExchange*` CAS witnesses + all-live real-FJP reference-local snapshots; `Fork6Hard 128 20` `GC_STRESS=65536` 48/48 clean for A4 signatures. | [fork6-fjp-multithread-jit-root-reclamation-FIXED.md](../internal/fixed-suite-bugs/fork6-fjp-multithread-jit-root-reclamation-FIXED.md) |
| **A4-gcstress** | Concurrent old-gen mark/sweep: SATB never wired (no production caller), young->old roots never traced, failed remark STW fell through to the sweep - three JIT-unrelated live-object-freeing races surfaced by the aggressive `GC_STRESS` lane | `docs/known-issues/repros/A4-fork6/Fork6Hard.java` + `CRATONVM_DBG_GC_STRESS=65536` | FIXED on dev (`57f545be`) - 42/42 focused `cratonvm-gc` unit tests; controls (`Fork6`, `Fork6Hard 256 40`, bt16) green. Later residual fixed separately. | [../internal/gcstress-concurrent-oldgen-races-FIXED.md](../internal/gcstress-concurrent-oldgen-races-FIXED.md) (fixed) / [gcstress-residual-corruption-faces-FIXED.md](../internal/gcstress-residual-corruption-faces-FIXED.md) (residual fixed) |
| **A5** | **Object-binarytrees moving-GC corruption** вЂ” the compiled entry-point `main`'s JIT frame is invisible to `gc_quiescence` (invoked via `Vm::invoke` without a `JitEntryGuard`), so the **moving** young collector relocates its roots and can't rewrite the raw stack slots в†’ stale all-zero receiver. (The earlier "register-only stale root in `bottomUpTree`" framing was wrong вЂ” `bottomUpTree` isn't even compiled at the crash.) | [`../internal/repros/gc-stress-bintrees-main-args/`](../internal/repros/gc-stress-bintrees-main-args/) (`VAAload`) | вњ… **FIXED** (dev `77c98761`) вЂ” detect an unregistered JIT frame on the native stack в†’ non-moving sweep + full-stack mark. Repro archived under `docs/internal`. Residual: Windows-only (portable stack-bound is a follow-up) | [docs/internal/.../gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md](../internal/fixed-suite-bugs/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md) |

> ## вњ… FIX (2026-06-17, dev `32649b56`): **precise JIT oop maps, default-on** вЂ” closes the register-invisibility root-scan gap (A3)
> `CRATONVM_PRECISE_JIT_MAPS` is now **default-on** (opt out: `CRATONVM_NO_PRECISE_JIT_MAPS`).
> It makes the non-moving sweep's root scan **precise across every active JIT frame**
> (RBP-chain `frame_record` + per-safepoint oop maps + a conservative per-frame
> fallback), so a live oop in a callee-saved register of a **caller** frame вЂ” the
> register-invisible root the conservative deepest-band-only scan missed вЂ” is found.
> This fixes the **register-invisibility class** (A3 and the kafka bug-21/22 /
> tomcat-style register-invisibility reclaims). **Verified:** A3 (`MinRegexProbe`)
> green at `GC_STRESS` 524288 **and** 4 MB; `bintrees16/18` == golden
> `14985902`/`68332206` (no under-count); `matrix`/`sieve`/`fib` correct; opt-out
> reverts to the broken path.
>
> **Scope correction (verified 2026-06-17): A2 and A4 are NOT closed by this** вЂ” they
> have *separate* bugs. **[SUPERSEDED 2026-06-23: A2 is now FIXED вЂ” `6e3ddb05`, a
> free-list double-serve in the sweep coalescer, not a register root; `ReflRepro`
> `bad=0`. See the top-of-section refresh + the A2 table row. A4 is now FIXED by the 2026-07-09 real-FJP root/CAS/local-snapshot work.]**
> **A2** (`ReflRepro`) *(then)* still crashed with `implausible object
> size` / `inconsistent header` on the young-sweep WALK (a core array/String
> allocationв†”sweep bug, distinct from the register root вЂ” precise maps fix root
> *scanning*, not the sweep walker; it actually surfaced *more* of A2's corruption by
> retaining more вЂ” a symptom-timing effect, since cured). **A4** (`Fork6`, under the experimental `CRATONVM_REAL_FORKJOINPOOL`
> gate) now fails with a real-FJP `ForkJoinPool` CAS conflict on *both* precise-on and
> -off, masking the original reclaim вЂ” so unverified, not regressed.
>
> **Perf:** the per-invocation `frame_record` CALL was made ~40% cheaper by caching
> the top-frame RBP in a thread-local (`82cf85e9`): **fib44 2.5Г— в†’ 1.68Г—**; alloc/
> compute/array are neutral or slightly faster. Residual = the CALL itself (follow-up:
> inline the RBP store in codegen; the NOP-skip lever is unsafe вЂ” it anchors the frame
> walk; details in SB-CRASH-04 #5).
>
> **Regression sweep (2026-06-17, default-on vs `CRATONVM_NO_PRECISE_JIT_MAPS`):**
> **zero correctness regressions** вЂ” `bintrees10/12/14/16/18`, `matrix600/800`,
> `sieve250k`, `fib44` all checksum-identical; 10 standalone app probes
> (`AR`/`Antora`/`CHMEq`/`CollCopy`/`DOMWalk`/`Builtins`/`CPUtil`/`Asm`/`ArrInst`/`AnonM`)
> byte-identical output to legacy. The full 50+ app gauntlet remains the CI bar.
> Predecessor: the `SHADOW_STACK` reload SIGSEGV fix (`19fd6707`).

The history below predates the fix.

> **Correction (2026-06-17):** A2 was briefly believed fixed via a "JIT-scan-cache
> is unsound в†’ default-OFF" change (`8dfd5c2b` / `bug-06b`). That was a **GC-timing
> mask, not a fix**, and was **reverted** (`b41c0484`): on current `dev` ReflRepro
> crashes identically cache-on and cache-off. `bug-06b-jit-scan-cache-unsound.md` is
> superseded by the reflrepro handoff (the genuine `collection_count` cache key it
> added was kept).

> **Verified on current `dev` (binary built 2026-06-17):** for the A3 repro
> (`MinRegexProbe code 20000`, `CRATONVM_DBG_GC_STRESS=524288`), the *only*
> correct config is `--nojit`. Every conservative trick
> (`CRATONVM_NO_JIT_SCAN_CACHE`, `CRATONVM_JIT_SAFEPOINT_REG_SPILL`,
> `CRATONVM_DBG_FULLSTACK_SCAN`, and combinations) still crashes вЂ” confirming the
> root is genuinely register-resident and unreachable by any stack scan. Every
> `CRATONVM_SHADOW_STACK` variant is also currently broken (movable в†’ SIGSEGV;
> `+pin` в†’ hang; `+noreload` в†’ hang). See SB-CRASH-04 for the live signatures.
>
> **Note the asymmetry:** `CRATONVM_SHADOW_STACK` *fixes* A2 (`ReflRepro` вЂ” no
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
| **MT-STW** | `Thread.join` monitor-ownership **desync under concurrent GC** (JIT-off): `monitor_wait`/terminate-tail used a raw `ObjectRef` captured before blocking, then `arrive_and_wait` let the in-flight STW relocate+zero it в†’ `ensure_inflated` on the stale address synthesised a fresh `owner=None` monitor в†’ IMSE in the javac synchronized-exit loop в†’ joiner livelock в†’ STW wedge. ~1вЂ“3% on `scratch_churn/Churn.java`; the tail after the four barrier+expansion fixes (`68c6993e`, 0%в†’~97%). | вњ… **FIXED** (`74b195b4`) вЂ” remap the receiver through `arrive_and_wait`'s returned pointer map; Churn 0 hangs / ~480 runs | [../internal/mt-stw-join-monitor-desync.md](../internal/mt-stw-join-monitor-desync.md) |

> Bug **B** (JUnit `@Timeout` "interceptor invoked twice") is **FIXED** and archived вЂ” it was
> never threading/`MethodHandle`: a synthetic natural-order compare raised `NoSuchMethodError`
> instead of `ClassCastException` for a non-`Comparable` element, escaping Spring's `catch (CCE)`
> and tripping JUnit's chain detector. See [`docs/internal/fixed-suite-bugs/spring-bug-04-junit-timeout-interceptor-double-proceed.md`](../internal/fixed-suite-bugs/spring-bug-04-junit-timeout-interceptor-double-proceed.md).

## Standalone вЂ” bug-06 assertion-mismatch family (Spring suite reflection/annotation tail)

The bug-06 census (`spring-suite/crash-reports-2026-06-16/bug-06-assertion-mismatch-families.md`)
clustered ~529 genuine assertion mismatches into 6 families. Families 1вЂ“5 are closed
(field-updaters `fe52db3a`; `HttpClient.executor`; synthetic-`Object` superclass `40b6d94a`;
`findLoadedClass` no-load `4b923e86`; F5 extinct 2026-07-02, all on `dev`). The one open family is
annotation-synthesis **native-return-value** correctness, not GC/JIT:

| # | Bug | Status | Doc |
|---|---|---|---|
| **F5** | Reflection native returns `null` where HotSpot returns a `Class`/`Method` (`getDeclaredMethod on null` Г—28). Common paths **verified clean** (`Refl5` == HotSpot); the Г—28 aggregate was a cross-family cascade and is **extinct** вЂ” 0 instances in the clean 2026-06-30/07-01 full re-runs and a fresh 196-class nojit+jit sweep on dev `ffb247e5`. **Do not** touch `synthetic_class_mirror` slot 0 (refuted hypothesis). | вњ… **CLOSED 2026-07-02** вЂ” failcause extinct, nothing to attribute | [bug06-fam5-reflection-getdeclaredmethod-null.md](../internal/fixed-suite-bugs/bug06-fam5-reflection-getdeclaredmethod-null.md) (moved) |
| **F6** | Spring annotation **synthesis** (`@AliasFor`/`MergedAnnotation`/`MirrorSets`) value mismatches (the `AnnotationUtilsTests` "~2 GB OOM" half was the `toArray` self-recursion, now **FIXED**). | рџ”ґ **OPEN** вЂ” `[[spring-bug-01]]` umbrella | _(doc removed; tracked on branch `fix/bug06-fam6-repeatable-merge`)_ |

## Consolidated suite bug docs (open & distinct вЂ” copied in 2026-06-18)

Genuine open VM defects pulled in from the suite-specific trackers
(`spring-suite/crash-reports-2026-06-16/`, `spring-suite/bugs/`,
`docs/kafka-suite-0617/`) so this folder is the single map. **These are copies** вЂ”
the originals remain in their suite folders (which keep their own numbering). Only
open, distinct bugs were copied; FIXED docs (bug-03, crash-01/02/03,
spring-bug-02/03/04/05/09/12, kafka bug-A вЂ” see `docs/internal/fixed-suite-bugs/`)
and already-consolidated ones (fam5/6) were left in place.

| Bug | Category | Status | Doc |
|---|---|---|---|
| String constant corrupted в†’ `Object` under load | VM-CORRECTNESS / GC | вњ… **RESOLVED / NOT REPRODUCED 2026-06-20** вЂ” a 45-class single-JVM spring-core batch on dev ran clean (0 `status=java.lang.Object`, all OK, rc=0); Family-A precise-maps default-on + `toArray` recursion fixed. Doc removed (writeup in git history). | _(removed)_ |
| `MergedAnnotations` hang | VM-HANG | вњ… **FIXED 2026-06-20** (`8795b88d`) вЂ” was the `ReferencePipeline.toArray(IntFunction)` self-recursion; `MergedAnnotationsTests` now 174/178 (residual 4 = synthesis mismatch, cf. bug-06 F6) | [toArray-recursion fix](../internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md) |
| Serializable proxy round-trip | VM-CORRECTNESS (proxy + serialization) | вњ… **RESOLVED on `dev`** вЂ” the standalone serializeв†’deserialize JDK-proxy repro round-trips correctly; `SerializableTypeWrapperTests` generic-type-render residual tracked on branch `fix/generic-array-type-tostring`. Doc removed. | _(removed)_ |
| JUnit-platform execution `LoadError` | VM-CORRECTNESS / dispatch | рџ”ґ **OPEN** вЂ” JUnit platform internals; Family-A GC-root race (NOT related to the now-fixed bug-04, which was a non-`Comparable` compare exception-type bug, not a GC race) | [spring-bug-10-junit-platform-execution-loaderr.md](../internal/spring-bug-10-junit-platform-execution-loaderr.md) |
| Groovy / scheduler crashes (rc=139) | VM-CRASH | рџџЎ **PARTIAL** вЂ” Groovy SIGSEGV fixed via the bug-12 HashMap-layout fix; residual = a separate Groovy **hang at BEGIN** (inventory, needs per-cluster trace) | [spring-bug-11-groovy-and-scheduler-crashes.md](../internal/spring-bug-11-groovy-and-scheduler-crashes.md) |
| Mockito `mockStatic` + mock dispatch | VM-CORRECTNESS (Mockito dispatch) | вњ… **FIXED on `dev`** вЂ” the dispatch/shadowing half landed earlier, and the stale JIT call-site residual was fixed by redefine-time compiled dispatch quiescing. | [fixed residual](../internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md) |
| `WeakHashMap` stream infinite hang | VM-HANG в†’ JIT codegen | вњ… **FIXED on `dev`** (`1cd0ab26`, JIT ban; verified; doc removed). The broader callee-saved GPR local-home family is now retired by the default-off fix and archived in the [JIT regalloc family doc](../internal/jit-regalloc-callee-saved-clobber-family.md). | _(removed)_ |

> `spring-bug-10` is a **Family A** (GC-root-coverage-under-JIT) manifestation seen from the
> Spring suite вЂ” same root cause as A1вЂ“A4 above, a different entry point. Fixing precise JIT
> stack roots should clear it; tracked there. (`springsuite-bug-04` was the same race but no
> longer reproduces вЂ” doc removed.)

## The springrepos handoff (mostly fixed)

[`springrepos-extension-hang-jit-throughput-and-deep-recursion.md`](../internal/springrepos-extension-hang-jit-throughput-and-deep-recursion.md)
is the archived multi-defect handoff for `SpringRepositoriesExtensionTests`. The hang,
parse-NPE (#1), generics (#2), and the indy `MethodHandle.type()` layers
(3/3b/3c/3d) are all **fixed**, and **layer 3e is now вњ… FIXED on dev** (`7335f918`)
вЂ” the test is **11/11 FULL GREEN**. The 3e writeup (the Groovy indy call on a
**Mockito mock**, `this.repositories.maven { вЂ¦ }`) moved to
[`docs/internal/spring/spring-boot-groovy-indy-mockito-mock-dispatch.md`](../internal/fixed-suite-bugs/spring/spring-boot-groovy-indy-mockito-mock-dispatch.md).
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

- **Hibernate JAXB class-load storm** (HIB-DEV-03) вЂ” вњ… FIXED (`fix/hib-dev-03-jaxb-classload`): the
  synthetic-stub "upgrade" re-ran a full O(num_jars) classpath scan on every map-node allocation; memoizing
  the known-absent result dropped a 20k-node put loop 20 120 ms в†’ 132 ms.
- **Hibernate JTA cluster** (Narayana XA + socket loopback) вЂ” вњ… RESOLVED. L0 `getInetAddress` NPE fixed on
  dev (`ada6cebf`); L1 XA-completion was never broken (refuted); L2 was a process-wide `accept()` deadlock +
  `getLocalPort()==0`, fixed on `fix/hib-jta-xa-loopback`. в†’ [`docs/internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md`](../internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md).
- **Hibernate JAXB/ByteBuddy bootstrap slow** вЂ” вњ… RESOLVED / no-repro (`1db07c35`/`25c42e13`). в†’ [`docs/internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md`](../internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md).
- **JUnit 5 `@ExtendWith` meta-annotation `ParameterResolver`** вЂ” вљ пёЏ MISDIAGNOSED / does-not-reproduce;
  annotation discovery is byte-identical to HotSpot and `JtaCustomAfterCompletionTest` passes 5/5. в†’ [`docs/internal/junit5-extendwith-meta-annotation-parameterresolver.md`](../internal/junit5-extendwith-meta-annotation-parameterresolver.md).

## Standalone вЂ” HQL parser rejects chained additive/duration/concat operators

вњ… **FIXED and MERGED to `dev`** (`3864097b`, `fix/hql-chained-operator-parse`,
commit `4e2a4493`); fully re-verified 2026-07-04 (all 3 affected classes now
100% pass, no regressions). Moved to
[`docs/internal/hql-antlr-chained-operator-syntax-error-FIXED.md`](../internal/hql-antlr-chained-operator-syntax-error-FIXED.md).
`a + b + c`
(or `a || b || c`, chained date/duration arithmetic, etc.) failed to parse вЂ”
CratonVM-only, second occurrence of the same operator class rejected with
ANTLR `SyntaxException: no viable alternative`. **NOT the same bug as
`jit-deep-recursion-fault-recovery.md` (Bug C)** вЂ” reproduced identically
with `--nojit` (Bug C is JIT-only) and with a trivial standalone ANTLR4
grammar unrelated to Hibernate/Groovy. Root-caused to a one-line defect in
CratonVM's native Rust reimplementation of `ParserATNSimulator`'s closure
algorithm (passed `inContext = !full_ctx` instead of `depth == 0`), a
generic defect in revisiting the same parser decision
twice within one parse; the specific defective method is not yet identified.

## Standalone вЂ” Hibernate deserialized SessionFactory is null

[docs/internal/hibernate-deserialization-sessionfactory-reconnect-null.md](../internal/hibernate-deserialization-sessionfactory-reconnect-null.md)
вЂ” full-suite census (dev, 2026-06-17). 5 serialization round-trip tests NPE
(`getMappingMetamodel`/`getClassLoaderService` on null) because a deserialized
`EntityManager`/`SessionFactory` doesn't reconnect to the live factory. Generic
`readObject`/`readResolve` work on CV (verified); the gap is Hibernate's
`SessionFactoryRegistry.findSessionFactory(uuid,name)` returning null after deser. рџ”ґ open.

## Hibernate full-suite census (dev 2026-06-17) вЂ” additional docs

Per-run bug reports from the (gitignored) `apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/`.
(The JSON-function `al_state` SIGSEGV and the reversed stack-trace order were **FIXED** and their docs
removed; the JTA `getInetAddress` per-class report was consolidated into the resolved JTA doc in
[`docs/internal/`](../internal/).)
- [hibernate-hang-clusters-summary.md](../internal/hibernate-hang-clusters-summary.md) вЂ” census overview (now in `docs/internal/`); **H1/H2/H3 resolved 2026-06-20**, **H4 root-caused** (its own open doc below).
- HQL/ANTLR parser census H4 (`function.json.JsonArrayUnnestTest`) is not a
  separate open issue file anymore. It is consolidated into
  [jit-deep-recursion-fault-recovery.md](jit-deep-recursion-fault-recovery.md)
  as a second reproducer for the same cold ANTLR prediction
  throughput / JIT backend-coverage cluster.

Also fixed on dev this run (no standalone doc вЂ” see commit): `Locale.toLanguageTag()` dropped all subtags for real Locales (`13e8c761`).

- **Hibernate wrong-result assertion failures** вЂ” cluster of CV-only wrong-result assertion FAILs (UniqueConstraintBatching 1-vs-0, DetachedBag true-vs-false, EntityGraphBatchSize, immutable+converter deser, вЂ¦); each likely a separate root cause. рџ”ґ open (handoff; standalone doc not preserved).

## Hibernate suite residuals (2026-06-24)

Triaged while fixing the collection-delegation native stack overflow (`7b224d8a`:
`try_delegate_real_collection` self-recursion в†’ `EXCEPTION_STACK_OVERFLOW` building a
`SessionFactory`). Once that crash was fixed, three pre-existing CratonVM-only
residuals surfaced (all fail identically at baseline `b0aab8f9`, so none is from the
regression):

- [hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md](../internal/fixed-suite-bugs/hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md) -
  вњ… **FIXED/RETIRED 2026-07-08.** The original `ProxyClassReuseTest.testNoReuse`
  loader-blind `CONSTANT_Class` bug remains fixed (3/3 pass), and the residual
  Spring/Groovy and BeanShell tails that kept the note active now pass:
  `GroovyBeanDefinitionReaderTests` 36/36, `BshScriptFactoryTests` 18/18,
  namespace/component-scan probe green, and related `InPredicateTest` default-JIT
  run green with no `DomainParameterXref.removeEldestEntry` NSME. Same family as
  **SBR-14** / `SC-custom-classloader`; historical details are archived under
  `docs/internal/fixed-suite-bugs`.
- [hib-bytecode-enhancement-loader-faithful-linking.md](hib-bytecode-enhancement-loader-faithful-linking.md) вЂ”
  рџ”ґ **REOPENED 2026-07-04** (moved back from `docs/internal`, where it was mis-archived as
  "FIXED / ARCHIVED"). Builds on the proxyclassreuse fix above with superclass/interface linking,
  `invokespecial` owner dispatch, and SessionFactory-build loader-identity fixes вЂ” all confirmed
  genuinely landed on `dev` by a fresh source audit. But `enhancement.lazy.*`/`mapping.lazytoone.*`
  remain the single largest open Hibernate-enhancement gap (a fresh 18-class sample: 2/18 PASS,
  14/18 FAIL, 1/18 HANG), not the "separate residual" the archived doc implied.
- [hib-sortnatural-persistentsortedset-cascade-drop.md](../internal/fixed-suite-bugs/hibernate/hib-sortnatural-persistentsortedset-cascade-drop.md) вЂ”
  вњ… **FIXED on dev** (`9ac7ef1d`; moved to docs/internal). `SortNaturalTest` (`sorted.set`/`sorted.map`): a cascaded `@OneToMany SortedSet`
  drops an element on **persist** (only 1 of 2 rows inserted; `size()` 2в†’1). Core `TreeSet` verified
  correct on every access path; the loss is in the Hibernate `PersistentSortedSet` cascade
  interaction. Part of the HIB-CV-35 cvonly long-tail.
- [hib-nodepth-shrinkwrap-par-archive-url.md](../internal/fixed-suite-bugs/hibernate/hib-nodepth-shrinkwrap-par-archive-url.md) вЂ”
  вњ… **FIXED on dev** (`9258f821`; moved to docs/internal). `NoDepthTests` JPA variants: ShrinkWrap in-memory `.par` archive +
  `ShrinkWrapClassLoader` need a custom `URLStreamHandler` ("Could not create URL for archive");
  the 2 non-JPA variants pass.

## Test-suite repair findings (2026-06-21)

While repairing the in-repo test suites (most failures were stale tests / missing
fixtures / a wrong feature set вЂ” all fixed on `dev`), three defects were left open
because each needs a risky core change or a large quality pass. One of the three (the
JIT divide-by-zero re-run) has since been fixed; the other two remain open:

- вњ… **JIT `idiv`/`irem` divide-by-zero re-runs the whole method** (side effects double-execute):
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

- вњ… **Mixed JUnit 5.10.3/6.0.3 runtime on `kc-universal-cp.txt`** вЂ” FIXED (local
  classpath file normalized to a single JUnit 6.0.3 stack). Caused 338 `CRASH` rows
  (`NamespaceAwareStore.computeIfAbsent` `NoSuchMethodError`) across `tests/base`
  and `tests/clustering`. Historical record moved to
  [../internal/keycloak-junit-namespaceawarestore-classpath-crashes.md](../internal/keycloak-junit-namespaceawarestore-classpath-crashes.md).
- вњ… **`Assert.assertNotNull` linkage crash (37 `CRASH` rows, `testsuite/model`)** вЂ”
  FIXED (`smallrye-common-constraint-2.16.0.jar` was entirely absent from
  `kc-universal-cp.txt`; added). Also added: a `CRATONVM_TRACE_UNIMPLEMENTED`-gated
  diagnostic that names the missing class whenever CratonVM's classloader falls
  back to an empty synthetic stub for an unresolvable `org/jboss/`, `org/wildfly/`,
  `io/quarkus/`, `io/smallrye/`, вЂ¦ class, plus a hint on the terminal
  `NoSuchMethodError` warning when the target class is such a stub вЂ” so this class
  of masked-classpath-gap bug self-diagnoses next time instead of needing a
  multi-hour investigation. Historical record moved to
  [../internal/keycloak-smallrye-assertnotnull-linkage-crashes.md](../internal/keycloak-smallrye-assertnotnull-linkage-crashes.md).
- вњ… **`SmallRyeConfigBuilder.addDefaultSources` linkage crash (3 `CRASH` rows,
  `tests/db` + `tests/clustering`)** вЂ” FIXED (`smallrye-config`/
  `smallrye-config-common`/`smallrye-config-core` 3.16.0 and, one layer down,
  `microprofile-config-api-3.1.jar` were entirely absent from
  `kc-universal-cp.txt`; both added). Historical record moved to
  [../internal/keycloak-smallrye-configbuilder-defaultsources-linkage-crashes.md](../internal/keycloak-smallrye-configbuilder-defaultsources-linkage-crashes.md).
- вњ… **`org.keycloak.testframework.config.Config` Quarkus classpath gap** вЂ” FIXED
  (2026-07-06). `quarkus-core` and four more layers behind it (the full
  `smallrye-common-*` family, `org.ow2.asm:asm`, `jboss-logmanager`,
  `quarkus-bootstrap-runner`) were entirely absent from `kc-universal-cp.txt`;
  all added. `AccountConsoleDisabledTest` now runs past `Config.initConfig()`
  and Quarkus logging bootstrap into real JUnit 5 test execution. Also added
  `apps/keycloak-suite-runner/generate-kc-universal-cp.ps1` (a dry-run-by-default
  helper that pulls a named module's already-resolved `cratonvm-full-cp.txt`
  jars into `kc-universal-cp.txt`, per this doc's "no in-repo generator"
  ask) вЂ” see it for why a blind full-repo union isn't used by default.
  Historical record moved to
  [../internal/fixed-suite-bugs/keycloak-testframework-quarkus-config-classpath-gap.md](../internal/fixed-suite-bugs/keycloak-testframework-quarkus-config-classpath-gap.md).
- **`EnterpriseDbDatabaseSupplier` supplier discovery classpath gap** -
  FIXED (2026-07-08). The named `db-edb` class was present, but its
  `test-framework/test-containers` superclass and Testcontainers/Docker
  runtime jars were missing from the universal classpath. The generator now
  builds a filtered default runtime closure for the provider and prunes stale
  service-descriptor-only output dirs. Historical record moved to
  [../internal/fixed-suite-bugs/keycloak-testframework-enterprisedb-supplier-noclassdef.md](../internal/fixed-suite-bugs/keycloak-testframework-enterprisedb-supplier-noclassdef.md).
- **Keycloak universal classpath post-EnterpriseDB residuals** -
  FIXED (2026-07-08). The generator now adds filtered Selenium/UI and
  Quarkus Maven resolver closures, and the suite runner orders selected module
  output dirs before `kc-runner` for pathing jars so Keycloak resolves Maven
  artifacts from the correct module. Historical record moved to
  [../internal/fixed-suite-bugs/keycloak-universal-classpath-post-enterprisedb-residuals.md](../internal/fixed-suite-bugs/keycloak-universal-classpath-post-enterprisedb-residuals.md).

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
  instead of the real `IntStream` interface stamp вЂ” broke every IntStream op
  on `chars()`, not just `anyMatch`. Moved to
  `docs/internal/fixed-suite-bugs/`.
- ~~keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod.md~~ - FIXED
  2026-07-06: was 37 `CRASH` rows plus one abstract/no-test `EMPTY` row for the
  `testsuite/model` module. Root cause was `GlobalConfigurationBuilder.build()`
  being natively shimmed as an identity wrapper (returned `this` instead of a
  distinct `GlobalConfiguration`), so `isClustered()` correctly-but-confusingly
  NoSuchMethodError'd against the Builder's genuine runtime class. Moved to
  `docs/internal/fixed-suite-bugs/keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod-FIXED.md`.
- ~~keycloak-model-infinispan-configurationbuilder-classcastexception.md~~ -
  FIXED 2026-07-06: same identity-wrapper bug shape one step deeper in the
  same path (`ConfigurationBuilder.build()`). Fixed by reworking
  `native_dcm_define_configuration` to read size/ttl via `Configuration`'s
  real accessor API (`memory().maxCount()` / `expiration().lifespan()`)
  instead of a raw synthetic slot index, then removing the identity-wrapper
  native the same way as `GlobalConfigurationBuilder.build()`. Moved to
  `docs/internal/fixed-suite-bugs/keycloak-model-infinispan-configurationbuilder-classcastexception.md`.
  `RealmModelTest` then reached a residual initially misdiagnosed as a Netty
  setAccessible bug; the actual cause was
  [Infinispan JGroupsTransport.start() never invoked](../internal/fixed-suite-bugs/keycloak-model-jgroupstransport-start-never-invoked-FIXED.md),
  now FIXED. `RealmModelTest` then reached a distinct residual,
  [Infinispan Cache.config null after real DefaultCacheManager.start()](../internal/fixed-suite-bugs/keycloak-model-infinispan-cache-config-null-after-real-start-FIXED.md),
  now ALSO FIXED (2026-07-06); the historical JIT-only decode-error note is
  retired, the current open follow-up is the `RealmModelTest` Liquibase-phase
  timeout, and the sibling `--nojit` STW shutdown hang is retired to
  [keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md](../internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md).
- [keycloak-sssd-system1-findbootstrapclassornull-nosuchmethod.md](keycloak-sssd-system1-findbootstrapclassornull-nosuchmethod.md) -
  2 `FAIL` rows in the SSSD module, missing
  `java/lang/System$1.findBootstrapClassOrNull(String)Class`.

## Consolidation log

- **2026-06-17:** Merged `precise-jit-stack-maps-multithread-fjp-worker-testcase.md`
  (handoff/testcase) and `precise-jit-stack-maps-fork6-findings.md` (findings)
  into a single [fork6-fjp-multithread-jit-root-reclamation-FIXED.md](../internal/fixed-suite-bugs/fork6-fjp-multithread-jit-root-reclamation-FIXED.md)
  вЂ” they described the *same* Fork6 bug (A4). Added this index framing the
  A1вЂ“A4 family + standalone B/C, and recorded the current-`dev` A3 verification.

## Spring Framework full-suite run (2026-06-21) вЂ” archived

A full `spring-core` run (binary built from dev) surfaced a cluster of CratonVM
divergences. The historical suite index has moved to
[docs/internal/spring/spring-core-suite-2026-06-21.md](../internal/fixed-suite-bugs/spring/spring-core-suite-2026-06-21.md).
Most contained fixes from that sweep have landed on `dev`, including the stream
close-handler OOB, CHM null-key parity, LinkedHashMap null-replace behavior,
generic-bounds reification, keySet containment, StAX cursor natives, lambda-SAM
dispatch, JSpecify nullness reflection, and synthetic `Collector` SAM registration.

Residual handoffs from the sweep are retained in focused internal Spring notes:

- [SC-env-classreading.md](../internal/fixed-suite-bugs/spring/SC-env-classreading.md) вЂ” `Object.equals` override precedence and custom-CL resource-stream follow-up.
- [SC-resource-io-family.md](../internal/fixed-suite-bugs/spring/SC-resource-io-family.md) вЂ” remaining Resource/IO handoffs; several FileNotFoundExceptions are harness-CWD artifacts.
- [SC-stax-xml-family.md](../internal/fixed-suite-bugs/spring/SC-stax-xml-family.md) вЂ” namespace SAX-event-sequence mismatch after the fixed cursor-native/prefix items.
- [SC-task-retry-util-misc.md](../internal/fixed-suite-bugs/spring/SC-task-retry-util-misc.md) вЂ” Throwable deser, retry timing, ByteBuddy ClassInjector, and AQS throttle handoffs.
- [SC-misc-core-spring.md](../internal/fixed-suite-bugs/spring/SC-misc-core-spring.md) вЂ” SortedProperties OutputStream store handoff; CHM null-key methods are fixed on dev.
- [SC-hangs-mergedannotations-charsequence.md](../internal/fixed-suite-bugs/spring/SC-hangs-mergedannotations-charsequence.md) вЂ” two hangs: `MergedAnnotations.stream().toArray()` re-entry and Reactor `StepVerifier` producer scheduling.

Cross-cutting: ByteBuddy `ClassInjector$UsingReflection` failure breaks AssertJ
`assertSoftly` + Mockito; JUnit "TimeoutExtension multiple times" masks
underlying VM errors.

## Open bug docs relocated from `docs/internal/` (2026-06-22)

Still-**OPEN** bug docs are consolidated here so every unfixed bug lives under
`docs/known-issues/`. (A2/A4 are the `reflrepro-вЂ¦` / `fork6-вЂ¦` docs above;
FIXED/resolved bugs stay in `docs/internal/` вЂ” they are moved out only when fixed.)

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
Index + per-bug reports: [spring-boot-probe-sweep/INDEX.md](../internal/fixed-suite-bugs/springboot/INDEX.md).

**3 FIXED + merged to `dev`** (writeups in [`docs/internal/`](../internal/)):
- **SBR-03** (`a245002c`) strict array `instanceof` ([writeup](../internal/fixed-suite-bugs/springboot/SBR-03-array-interface-instanceof.md)) вЂ” `Object[] instanceof I[]` в†’ `false`.
- **SBR-06** (`bce39db7`) Constructor `Parameter.getParameterizedType()` generics
  ([writeup](../internal/fixed-suite-bugs/springboot/SBR-06-field-getgenerictype-raw.md)) вЂ” populate the Constructor mirror's `signature` field.
- **SBR-02** (`0d7dfc28`, merge `01375f90`) regex `replaceAll` / literal `replace` throughput wall
  ([writeup](../internal/fixed-suite-bugs/springboot/SBR-02-string-regex-throughput.md)) вЂ” flip `CRATONVM_NATIVE_STRING_REGEX`
  default-ON; `String.{replaceAll,replaceFirst,matches,replace(CharSequence,вЂ¦)}` route to fast
  Rust-regex natives. Was the `PluginXmlParserTests` hang.

**Open** (root-caused; none a safe one-liner вЂ” see each report):
- **SBR-01** Groovy `parseClass` hang Г—9 вЂ” see [SBR-01-groovy-parseclass-hang.md](../internal/fixed-suite-bugs/springboot/SBR-01-groovy-parseclass-hang.md); the overlapping buildSrc coldpath suite index is archived at [spring-boot-buildsrc-coldpath-hangs-2026-06-22.md](../internal/fixed-suite-bugs/springboot/spring-boot-buildsrc-coldpath-hangs-2026-06-22.md). **handoff**
- **SBR-14** custom `URLClassLoader(parent=null)` bypassed в†’ `AppClassLoader` (classloader isolation). **handoff**
- **SBR-04** annotation `getClass()`/`toString`; **SBR-05** `getDeclaredMethods` order (won't-fix, spec-unspecified);
  **SBR-07** `getSimpleName` (real defect = `getDeclaringClass0`/InnerClasses for Kotlin classes);
  **SBR-08/09/10/11/13** object-identity cluster (CV synthesizes JDK objects as abstract/base-typed вЂ”
  jar conn, NIO FS, IntStream, MethodHandle, ProtectionDomain); **SBR-12** `cratonvm.internal.UnmodifiableList`
  name leak (needs real `ImmutableCollections` or a guarded alias).
