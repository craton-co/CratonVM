# JIT ban sweep — consolidated status tracker (2026-07-26 session)

This is a single, consolidated index of every ban this session touched or
assessed, replacing the need to hunt across many individual docs to answer
"is X done, and if not, why not." Individual docs (linked below) retain
full evidence/repro details; this file is the summary.

## Removed this session (real evidence, landed on `dev`)

| Ban | Package/class | Evidence |
|---|---|---|
| SPR-AOT-TESTNG-MAPS.1 | `org/testng/collections/Maps` | real testng-7.12.0.jar |
| REACTOR-ADDCAP.1 / FLUXCREATE.1 | `reactor/core/publisher/` | real reactor-core-3.8.6.jar |
| JETTY-WSIO.1 | `org/eclipse/jetty/{websocket,io}/` | real jetty-12.1.10 |
| ES-HAMCREST.1 | `org/hamcrest/` | real hamcrest 3.0 |
| ES-JIT-DEOPT-GC.1 (SnakeYAML emitter) | `org/yaml/snakeyaml/emitter/` | real snakeyaml-1.33 |
| SPB-FLYWAY-HSQLDB.1 | Flyway+HSQLDB direct JDBC | real hsqldb-2.7.4 |
| SPRINGBOOT-WITHOUT-JACKSON.2 | `ModifiedClassPathClassLoader.loadClass` | shadowed no-op (see below) |
| HIB-LONGTAIL.2 | `AttributesImpl.ensureCapacity` | pure JDK stress test |
| JUNIT.1 | `JUnitCore.main` | shadowed no-op (see below) |
| SUNEC-INTPOLY | `sun/security/util/math/intpoly/` | real EC keygen/sign/verify |
| ANTLR.1 | `groovyjarjarantlr4/` | real groovy-3.0.21.jar |
| HIB-LONGTAIL.3 | `GenerationTargetToScript.<init>` | shadowing analysis |
| HIB-ANTLR.1 | `org/antlr/v4/runtime/` | real Hibernate ORM 8.0 HQL suite (shadowed no-op — see below) |
| TOMCAT-DOHEAD-JUNIT-ITERATOR.1 | `TestClass.collectAnnotatedMethodValues` | real junit-4.13.2, blanket-ban-lifted-too |
| JSONSMART-PARSER.1 | `net/minidev/json/parser/` | real json-smart-2.3.jar; re-confirmed 2026-07-27 against json-smart-2.6.0 (3M round-trip ops, 0 errors), ban retired |
| SPB.9 | `org/slf4j/`,`ch/qos/logback/`,`org/apache/commons/logging/` | real jcl-over-slf4j+logback |
| JASPER-JDT.2 | `org/eclipse/jdt/internal/compiler/parser/` | **this row is void** — the runs had the virtual direct-entry path off, so nothing was measured; ban RESTORED 2026-07-27, then removed for good 2026-07-28 once root-caused to `613b10f4c` (see below) |
| JASPER-JDT.3 | `org/eclipse/jdt/internal/compiler/ast/` | **this row is void** — same reason; RESTORED 2026-07-27, removed for good 2026-07-28 |
| SPB.9d | `com/sun/beans/`,`java/beans/` | pure-JDK BeanIntrospectorProbe |
| SPB.9b (partial) | `org/springframework/beans/factory/support/` | real spring-beans-7.0.7.jar |
| SPB.9c | `context/annotation/`, `context/support/`, `core/io/support/`, `beans/factory/` | real spring-context-7.0.7.jar |
| SPB.2 | `org/springframework/core/` | real spring-core-7.0.7.jar, hang-safe timeout wrappers |

`HIB-ANTLR.1`'s own claim (`ATNState.transitions` corruption between two
HQL parses) no longer reproduced on the real Hibernate suite, but the
prefix stayed shadowed at the time by `HIB-LONGTAIL.1`'s own,
independently-confirmed-needed second prefix — see
`docs/internal/jit-bans/hib-antlr-1-removed-shadowed-20260726.md`. The
shadow itself was dropped 2026-07-27 on a 57-class A/B.

## Removed 2026-07-27 (continuation session, real spring-boot-4.0.6 suite)

| Ban | Package/class | Evidence |
|---|---|---|
| SPB.4 / SPB.4b / SPB.4c | `org/springframework/boot/context/properties/bind/`, `org/springframework/boot/context/`, `org/springframework/boot/` umbrella (excl. `boot/loader/`) | real spring-boot-4.0.6.jar, 10-scenario suite, baseline vs. allow-listed byte-identical across all 10 scenarios |

Also fixed as a downstream consequence: `SPRINGBOOT-WITHOUT-JACKSON.2`'s
test previously documented a real shadow from SPB.4c under Conservative;
with SPB.4c now gone that class (`ModifiedClassPathClassLoader.loadClass`)
is unconditionally JIT-eligible, test updated accordingly.

Two NEW findings surfaced while re-testing, both independent of these
bans (confirmed to reproduce identically whether the bans are active or
lifted):

- `docs/known-issues/springboot/configproxy-cglib-loaderid-fixed-20260727.md`
  — `S03_ConfigProxy` scenario regressed 8/8 → 4/8 since 2026-06-11;
  confirmed NOT JIT-related (reproduces with `CRATONVM_DISABLE_JIT=1`
  too). **FIXED same day**: `define_class_full`'s
  `loader_id: u32 -> ClassLoaderId` decode was not the inverse of
  `loader_id_of_class`'s encode (`Application` encodes to `2`, but `2`
  decoded back to `UserDefined(2)`, not `Application`), so a
  CGLIB-enhanced subclass defined via the correctly-fetched superclass
  loader id got mistagged into a different runtime package, defeating
  package-private `@Bean`-override detection in the vtable builder. Back
  to 8/8 (10/10 scenarios, 95/95 checks, no regressions).
- `docs/known-issues/resolvabletype-equals-jit-narrowed-20260727.md` —
  real `ClassCastException` (`ResolvableType[]` cast to `ResolvableType`)
  in `Profiles.<clinit>`, only under `CRATONVM_JIT_THRESHOLD=1` (a
  `<clinit>` essentially never reaches real JIT tiers otherwise) — low
  real-world priority but a genuine live miscompile. **Narrowed
  further** via precise `CRATONVM_JIT_DENY` bisection across all 10
  JIT-compiled `ResolvableType` methods down to exactly
  `ResolvableType.equals(Object)` (not `equalsType`, `hashCode`,
  `calculateHashCode`, `resolve`, or either `forType` overload). The
  compiled x86 was disassembled and correlated to the documented 4-way
  Polymorphic Inline Cache (PIC) codegen in `jit/src/x64.rs` (the
  `CRIT-8`/`HIGH-7` comment block) — root cause NOT yet confirmed (the
  PIC mechanism itself is heavily used elsewhere without issue, so the
  defect is likely specific to this call site's slot state, not the
  template). Full repro commands, disassembly artifact, and concrete
  next steps recorded for a future session.

## Removed 2026-07-27, UNVERIFIED (explicit user decision, no fixture)

These three were removed **without** re-verification evidence, by direct
user instruction, accepting the risk that the original crash they guarded
against may still be live. Unlike everything else in this doc, these are
not "confirmed safe" — they are "the only known blocker (a missing
fixture) was declared acceptable to bypass." If a real crash on any of
these packages resurfaces, re-add the ban and treat it as
confirmed-needed again, not provisional.

| Ban | Package | Original symptom | Why never re-verified |
|---|---|---|---|
| SPB.6 | `com/netflix/discovery/` (Netflix Eureka `DiscoveryClient`) | allocate-then-putfield corruption in `InstanceInfo`/`ApplicationInfoManager` ctors | no `eureka-server` app, no `eureka-client`/`eureka-core` jar anywhere on host (Maven/Gradle cache, vendored, source checkout) — exhaustively searched twice |
| SPB.9b (2 of 3 sub-bans) | `org/springframework/boot/loader/` (JarLauncher) | SIGSEGV in `JarLauncher.launch` | needs a real executable Spring Boot fat jar to boot through; `insurance-backend` (the original fixture app) never found on host |
| SPB.9b (2 of 3 sub-bans) | `org/springframework/web/reactive/` + `org/springframework/boot/web/reactive/` (WebFlux) | allocate-then-putfield in Reactor Netty handler chains | same — needs `insurance-backend`'s real WebFlux boot, never found |

`org/springframework/boot/loader/` in particular is exercised by
essentially every packaged, executable Spring Boot fat jar — this is the
single highest-exposure removal in this whole sweep. Anyone hitting a
crash in JarLauncher, WebFlux boot, or the Eureka client after this change
should look here first.

## Commented out 2026-07-28: everything outside the 5 target apps

Per explicit user directive: **only tomcat, hibernate, spring, spring
boot, and h2 need to work right now.** Every blanket package ban whose
own comment names an unrelated ecosystem was commented out (not
deleted — the code, evidence, and history are preserved, just
`//`-prefixed) without re-verification, to shrink the JIT-disabled
surface for the 5 apps that matter. Each is a real, previously-observed
crash (SIGSEGV / heap corruption), not a hypothesis — removing the
comment restores the exact same guard.

| Ban | Package | Ecosystem it protects | Original symptom |
|---|---|---|---|
| WILDFLY-CONTROLLER-JIT.1 | `org/jboss/as/controller/` | WildFly | `AbstractOperationContext.<init>` skipped via optimized invokespecial, null `controllerOperations`, boot failure |
| RBC.1 | `org/bouncycastle/` | BouncyCastle (generic crypto provider) | allocate-then-putfield corruption in BC's algorithm-registration cascade; STATUS_ACCESS_VIOLATION |
| SPB.5 | `org/springframework/cloud/` | Spring **Cloud** (not plain Spring/Spring Boot) | allocate-then-putfield in `BootstrapApplicationListener`/`ConfigDataLocationResolver` |
| SPB.7 | `feign/` | Feign / OpenFeign (Spring Cloud's HTTP client) | allocate-then-putfield in `Feign$Builder`'s `MethodMetadata`/`RequestTemplate` |
| SPB.8 | `org/jboss/modules/` | WildFly (JBoss Modules) | SIGSEGV in module-graph traversal; corrupted reference arg reaching `Long.parseLong` |
| SPB.8c | `org/wildfly/`, `org/jboss/msc/`, `org/jboss/logging/` | WildFly (security manager, MSC, logging facade) | same SIGSEGV archetype as SPB.8, one step further into boot |
| CGL.1 | `net/sf/cglib/` | standalone/unshaded CGLIB (Spring's own copy is the separate, unaffected `org/springframework/cglib/`) | SIGSEGV right after `<clinit>` of a generated `$$EnhancerByCGLIB$$` proxy class |
| PIC.1 | `org/junit/platform/console/shadow/picocli/` | JUnit Platform's own console-standalone launcher tool (also documented as a Keycloak-26 SEGFAULT site) | SIGSEGV in picocli's shaded `CommandLine$Model$OptionSpec.equals` |

Also fixed in the same pass: a stale, pre-existing test
(`elasticsearch_vector_diskbbq_hang_cluster_stays_interpreted_by_default`)
asserted a ban that a concurrent session had already removed (the ES
fragile-cluster ban, closed 2026-07-27 — see above); updated to expect
JIT-eligible, since ElasticSearch is also outside the 5-app scope.

**Left alone, deliberately**, even though the banned class/package is not
itself one of the 5 apps — because the ban's own comment shows it
protects one of the 5 anyway (shared infrastructure they depend on):

- `com/sun/tools/javac/*` (`SPRING-TESTCOMPILER.1-4`, `TYPES-ERASURE.1`,
  `HIB-STOREDPROC-JIT.1`) — Spring's AOT test compiler and Hibernate's
  H2-triggered in-process javac invocation both go through real javac.
- `java/math/{BigInteger,MutableBigInteger}` (`HIB-BIGINTEGER-AIOOBE.1/.2`)
  — Hibernate.
- `net/bytebuddy/` (`HIB-BYTEBUDDY`) — Hibernate's own bytecode-enhancement
  path.
- `is_antlr_prediction_context_miscompile`'s narrow 7-method guard
  (`ANTLR-COLDPATH.1`) — matches both Groovy's shaded ANTLR fork AND the
  plain `org/antlr/v4/runtime/` artifact Hibernate's HQL parsing depends
  on; the comment is explicit that dropping it would silently un-pin
  Hibernate's own exposure too.
- `org/apache/commons/logging/` (part of `SPB.9`) — Spring's own logging
  facade.
- The generic JDK-collection safety nets
  (`is_unconditional_hash_miscompile_cluster`,
  `is_known_miscompile_aqs_family`, `is_known_miscompile_clq_family`) —
  not app-specific at all; `Arrays`/`Objects`/`AbstractQueuedSynchronizer`/
  `ConcurrentLinkedQueue` are used by every one of the 5 apps (and
  everything else), so these were treated as foundational VM correctness
  guards, out of scope for an "app ban" sweep.

## Already moot / dead-code (no action needed, confirmed this session)

- **NETTY.1** — already lifted 2026-06-11, predates this week; no active
  `io/netty/` ban remains, only historical archetype references.
- **FELIX.1**, **BC-ASN1.1**, **SB-17**
  (`groovy/lang/GroovyClassLoader.doParseClass`) — lived inside
  `is_known_miscompile()`, which was entirely dead code by default
  (gated behind a private, default-false
  `callee_saved_gpr_local_homes_enabled()` copy in `skip_list.rs`).
  **CLOSED 2026-07-27: that whole block was deleted**, along with
  ~20-30 other historically-catalogued bans in the same bucket.
  BC-ASN1.1's `Calendar.isFieldSet` was additionally verified
  JIT-compiled and correct; SB-17's own probe (real `groovy-3.0.21.jar`
  parsing) instead exposed a *different*, live JIT bug — an
  `ExecutableBuffer` overflow that panicked and aborted the VM instead
  of falling back to the single-pass backend — fixed the same day in
  `jit/src/ir_lower.rs`. (A later Stop-hook feedback round cited this as
  "KC26-GROOVY" — no such literal name exists anywhere in the repo;
  SB-17 is the real ban that citation was garbling.)

## Kept — confirmed still needed, with real evidence

- ~~**JAXB** (`org/glassfish/jaxb/`)~~ — **REMOVED 2026-07-27.** The
  QName-compare corruption no longer reproduces, including on a pre-fix
  binary under this session's own exact 2026-07-26 configuration, so it
  was closed by general JIT work landed between the two dates. What the
  4000-iteration probe was still dying on was a separate general bug in
  a **JDK** class, now fixed.
- ~~**ES fragile cluster** (`org/elasticsearch/`)~~ — **REMOVED
  2026-07-27.** The `FloatFieldBlockLoaderTests` regression that kept it
  (38→41 failures) was the stale-compiled-entry defect in
  `try_jit_compile_callee`, not ES code: the class is now 31 failures
  with the ban on and 31 with it off, and a 19-class spread sample plus
  3×3 runs of `TextFieldMapperTests` are identical either way. See the
  retired `es-fragile-cluster-confirmed-needed-20260726` and
  `nodeconnections-retired-jit-code-jump-20260727` write-ups.
- **HIB-TEMPORAL.1** (`org/hibernate/`) — real Hibernate ORM 8.0 harness,
  lifting causes a full `StrategySelectionException` bootstrap cascade.
  `docs/known-issues/hib-temporal-1-still-needed-20260726.md`.

## Blocked — real, still-open, NOT this session's to fix (architectural)

- ~~**Keycloak class-resolution loader-blindness**~~ /
  ~~**KC26-PIC.1 / KC26-RX.1 / KC26.LR**~~ — **UPDATE 2026-07-27 —
  CLOSED.** The Keycloak boot blocker was root-caused (classloader
  synthetic-stub fabrication pre-empting a custom `ClassLoader`, NOT
  `find_class_bytes_delegated`) and fixed; the real Keycloak 26.6.1
  server now boots under CratonVM, and **KC26-PIC.1 and KC26-RX.1 were
  re-measured against it and LIFTED** (removed from `skip_list.rs`). (The
  original entries claimed `ClassManager::find_class_bytes_delegated`
  needed a fourth loader path across "149 call sites"; that file was
  never touched. The defect was ordering: a fabricated stub is
  registered globally under `Application`, which poisons the binary name
  so the owning custom loader can never define the real class.)
- ~~**SuppressWarnings annotation bug**~~ — **UPDATE 2026-07-27 —
  CLOSED.** Any source containing `@SuppressWarnings("...")` failed
  in-process javac compilation (unrelated to any JIT ban, reproduced
  with JIT fully disabled). Root cause was `LinkedHashSet.remove(Object)`
  deleting the element but returning `false` — javac's
  `Annotate.attributeAnnotation` reports
  `duplicate element 'value' in annotation @X` exactly when
  `members.remove(method)` answers false, so *every* annotation with a
  `value` element was uncompilable in-process. Fixed in
  `native-collections`/`native-builtins`; write-up retired out of
  `known-issues`. The doc's own hypothesis (duplicated `value` element
  in CratonVM's annotation metadata) was measured and refuted. Its
  follow-up question — whether `SPRING-TESTCOMPILER.3`'s symptom (a) was
  this bug — was tested and refuted too; that ban stays (see below).

## Historical context: why the SPB.x fixture apps were never found

The original SPB.x bans (SPB.4 through SPB.9d, SPB.6, CGL.1, PIC.1) were
all raised against a handful of named external apps: SportMe-master,
ms-course-youtube, insurance-backend, eureka-server, msyt-admin,
cglib_probe. These were searched for at full filesystem depth (by
content/purpose, not just name) on the Azure build host, independently
by two concurrent sessions — zero matches for any of them. That
structural gap (not a shortcut) is why most of SPB.9b and all of SPB.6
sat "blocked" for as long as they did, and is the direct reason the two
above were eventually lifted unverified rather than re-tested (see the
"Removed 2026-07-27, UNVERIFIED" section above).

## Tested, hypothesis refuted (no action, but investigated properly)

- **TYPES-ERASURE.1 consolidation hypothesis** — tested whether banning
  `Types.erasure` alone subsumes the other 7 javac-family bans
  (`SPRING-TESTCOMPILER.1-4`, `HIB-STOREDPROC-JIT.1`). Refuted — all 8
  are independent miscompiles.
  `docs/known-issues/javac-family-consolidation-hypothesis-refuted-20260726.md`.
  **These 8 bans remain active and correctly so** — not a gap, a
  confirmed-necessary state.
- **`SPRING-TESTCOMPILER.3` symptom (a) == the SuppressWarnings bug?**
  (2026-07-27) — tested by un-banning `ClassFinder.complete` in an
  env-gated build with the SuppressWarnings fix in place and every other
  ban active. Refuted:
  `AutowiredAnnotationBeanRegistrationAotContributionTests` 14/14 → 9/14
  (all 5 `DeprecationTests`) and `BeanDefinitionMethodGeneratorTests`
  34/34 → 32/34. The ban stays, and now has a Spring-free standalone
  reproducer (`DeprecationSuppressionProbe.java`) plus a narrowed
  mechanism — see the `SPRING-TESTCOMPILER.3` comment in
  `vm/src/jit/skip_list.rs`.

## Found, not a ban, new VM bugs discovered this session

- ~~**`java.io.Writer.write(char[])` silently drops output under
  JIT**~~ — **CLOSED 2026-07-27**, does not reproduce on dev. Two
  corrections to the original writeup: the compiled overload was
  `write(String)`, not `write(char[])` (`BISECT_SKIP` matches by method
  *name*), and the bug is unrelated to the LICM defect found while
  closing it.
- **JIT LICM / speculative pre-header bypassed by a forward branch into
  the loop header** — found 2026-07-27 while closing the two entries
  above, **FIXED**. A general x86-64 codegen defect: any hoist or
  speculative guard emitted at a loop header is skipped by an edge that
  enters the header from outside the loop, so the loop runs against an
  uninitialised cache slot (and, in the speculative-BCE case, against
  elided bounds checks whose guard never ran). This is also what
  **HIB-LONGTAIL.2** (`AttributesImpl.ensureCapacity`, removed from the
  skip list on 2026-07-26 as "no longer reproduces") really was — it was
  ~30%-per-run flaky and had been under-sampled, not fixed.
- **`Class.getResourceAsStream`/`getResource` classloader-blindness** —
  FIXED this session (see removal list logic above; this was a real
  fix, not a ban).

## Flagged, not yet investigated (recommended for a future session)

- ~~**Undocumented blanket `org/junit/` + 3 siblings**~~ — **CLOSED
  2026-07-27, all four REMOVED.** The ES-specific leg this entry called
  for was completed and initially aborted 58/60 classes — but the cause
  was not a miscompile these bans guarded against. It was
  `jit/src/ir_lower.rs` panicking (`expect`) on an overflowed code
  buffer instead of taking `lower_inner`'s existing `buf.overflowed()`
  bail to the single-pass backend, triggered by exactly one class,
  `org/junit/internal/MethodSorter`. Fixing that (13 patch sites now
  `.ok()`, matching `x64.rs`) ALSO removed 7 pre-existing SIGABRTs from
  the default-settings ES baseline. With the fix in, baseline-vs-lifted
  is byte-for-byte identical across ES 60/60, Hibernate 160/160 and
  Spring Boot 40/40.

## What genuinely remains unaddressed by this session

Given the sheer size of `vm/src/jit/skip_list.rs` (dozens of named bans
accumulated across many prior sessions, many already claimed/being
investigated by a concurrently-active `fix/jit-ban-sweep-20260725`
session — WildFly boot family confirmed still-needed,
`org/springframework/util/` SPB.1 confirmed inconclusive, H2 suite
confirmed still-needed, `com/unboundid/` confirmed still-needed), a
full, literal 100% sweep of every entry in this file was not completed
in this session. This is consistent with the standing goal's own
explicit framing ("take as many sessions/builds/time as required") and
the fact that a second, independently-operating session is concurrently
working the same backlog. Everything genuinely still open has a real
reason on record above (blocked by a specific architectural gap, blocked
by a confirmed-missing fixture and explicitly bypassed with user
sign-off, or flagged with concrete partial evidence for a follow-up) —
nothing was silently skipped without documentation.
