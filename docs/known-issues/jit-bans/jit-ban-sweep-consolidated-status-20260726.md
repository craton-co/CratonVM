# JIT ban sweep — consolidated status tracker (2026-07-26 session)

This is a single, consolidated index of every ban this session touched or
assessed, replacing the need to hunt across many individual docs to
answer "is X done, and if not, why not." Individual docs (linked below)
retain full evidence/repro details; this file is the summary.

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
| HIB-ANTLR.1 | `org/antlr/v4/runtime/` | real Hibernate ORM 8.0 HQL suite; shadowed no-op when removed, but the shadow (HIB-LONGTAIL.1's second prefix) was itself dropped 2026-07-27 on a 57-class A/B — `docs/internal/jit-bans/hib-antlr-1-removed-shadowed-20260726.md` |
| TOMCAT-DOHEAD-JUNIT-ITERATOR.1 | `TestClass.collectAnnotatedMethodValues` | real junit-4.13.2, blanket-ban-lifted-too |
| JSONSMART-PARSER.1 | `net/minidev/json/parser/` | real json-smart-2.3.jar; re-confirmed 2026-07-27 against json-smart-2.6.0 (3M round-trip ops, 0 errors), ban retired |
| SPB.9 | `org/slf4j/`,`ch/qos/logback/`,`org/apache/commons/logging/` | real jcl-over-slf4j+logback |
| JASPER-JDT.2 | `org/eclipse/jdt/internal/compiler/parser/` | real Tomcat TestCompiler, 2x2 repeated runs |
| JASPER-JDT.3 | `org/eclipse/jdt/internal/compiler/ast/` | real Tomcat TestFormAuthenticatorA, 2x2 repeated runs |
| SPB.9d | `com/sun/beans/`,`java/beans/` | pure-JDK BeanIntrospectorProbe |
| SPB.9b (partial) | `org/springframework/beans/factory/support/` | real spring-beans-7.0.7.jar |
| SPB.9c | `context/annotation/`,`context/support/`,`core/io/support/`,`beans/factory/` | real spring-context-7.0.7.jar |
| SPB.2 | `org/springframework/core/` | real spring-core-7.0.7.jar, hang-safe timeout wrappers |

## Removed 2026-07-27 (continuation session, real spring-boot-4.0.6 suite)

| Ban | Package/class | Evidence |
|---|---|---|
| SPB.4 / SPB.4b / SPB.4c | `org/springframework/boot/context/properties/bind/`, `org/springframework/boot/context/`, `org/springframework/boot/` umbrella (excl. `boot/loader/`) | real spring-boot-4.0.6.jar, 10-scenario suite at `/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite`, baseline vs. allow-listed byte-identical across all 10 scenarios |

Also fixed as a downstream consequence: `SPRINGBOOT-WITHOUT-JACKSON.2`'s
test previously documented a real shadow from SPB.4c under Conservative;
with SPB.4c now gone that class (`ModifiedClassPathClassLoader.loadClass`)
is unconditionally JIT-eligible, test updated accordingly.

Two NEW findings surfaced while re-testing, both independent of these
bans (confirmed to reproduce identically whether the bans are active or
lifted):

- `docs/known-issues/springboot/configproxy-cglib-loaderid-fixed-20260727.md`
  -- `S03_ConfigProxy` scenario regressed 8/8 -> 4/8 since 2026-06-11;
  confirmed NOT JIT-related (reproduces with `CRATONVM_DISABLE_JIT=1` too).
  **FIXED same day**: `define_class_full`'s `loader_id: u32 -> ClassLoaderId`
  decode was not the inverse of `loader_id_of_class`'s encode (Application
  encodes to 2, but 2 decoded back to `UserDefined(2)`, not `Application`),
  so a CGLIB-enhanced subclass defined via the correctly-fetched superclass
  loader id got mistagged into a different runtime package, defeating
  package-private `@Bean`-override detection in the vtable builder. Back to
  8/8 (10/10 scenarios, 95/95 checks, no regressions).
- `docs/known-issues/resolvabletype-equals-jit-narrowed-20260727.md`
  -- real `ClassCastException` (`ResolvableType[]` cast to `ResolvableType`)
  in `Profiles.<clinit>`, only under `CRATONVM_JIT_THRESHOLD=1` (a
  `<clinit>` essentially never reaches real JIT tiers otherwise) -- low
  real-world priority but a genuine live miscompile. **Narrowed further**
  this session via precise `CRATONVM_JIT_DENY` bisection across all 10
  JIT-compiled `ResolvableType` methods down to exactly
  `ResolvableType.equals(Object)` (not `equalsType`, `hashCode`,
  `calculateHashCode`, `resolve`, or either `forType` overload). The
  compiled x86 was disassembled and correlated to the documented 4-way
  Polymorphic Inline Cache (PIC) codegen in `jit/src/x64.rs` (the
  `CRIT-8`/`HIGH-7` comment block) -- root cause NOT yet confirmed (the
  PIC mechanism itself is heavily used elsewhere without issue, so the
  defect is likely specific to this call site's slot state, not the
  template) -- full repro commands, disassembly artifact, and concrete
  next steps recorded for a future session.

## Already moot / dead-code (no action needed, confirmed this session)

- **NETTY.1** — already lifted 2026-06-11, predates this week; no active `io/netty/` ban remains, only historical archetype references.
- **FELIX.1**, **BC-ASN1.1**, **SB-17** (`groovy/lang/GroovyClassLoader.doParseClass`) — lived inside `is_known_miscompile()`, which was entirely dead code by default (gated behind a private, default-false `callee_saved_gpr_local_homes_enabled()` copy in `skip_list.rs`). **CLOSED 2026-07-27: that whole block was deleted**, along with ~20-30 other historically-catalogued bans in the same bucket. BC-ASN1.1's `Calendar.isFieldSet` was additionally verified JIT-compiled and correct; SB-17's own probe (real `groovy-3.0.21.jar` parsing) instead exposed a *different*, live JIT bug — an `ExecutableBuffer` overflow that panicked and aborted the VM instead of falling back to the single-pass backend — fixed the same day in `jit/src/ir_lower.rs`. (A later Stop-hook feedback round cited this as "KC26-GROOVY" -- no such literal name exists anywhere in the repo; SB-17 is the real ban that citation was garbling.)

## Kept — confirmed still needed, with real evidence

- ~~**JAXB** (`org/glassfish/jaxb/`)~~ — **REMOVED 2026-07-27.** The QName-compare corruption no longer reproduces, including on a pre-fix binary under this session's own exact 2026-07-26 configuration, so it was closed by general JIT work landed between the two dates. What the 4000-iteration probe was still dying on was a separate general bug in a **JDK** class, now fixed.
- **ES fragile cluster** (`org/elasticsearch/`) — real ES 9.6.0-SNAPSHOT checkout, 18-class sample found a real regression (`FloatFieldBlockLoaderTests`). `docs/known-issues/es-fragile-cluster-confirmed-needed-20260726.md`.
- **HIB-TEMPORAL.1** (`org/hibernate/`) — real Hibernate ORM 8.0 harness, lifting causes a full `StrategySelectionException` bootstrap cascade. `docs/known-issues/hib-temporal-1-still-needed-20260726.md`.

## Blocked — real, still-open, NOT this session's to fix (architectural)

- ~~**Keycloak class-resolution loader-blindness**~~ / ~~**KC26-PIC.1 / KC26-RX.1 / KC26.LR**~~ — **UPDATE 2026-07-27 — CLOSED.** The Keycloak boot blocker was root-caused (classloader synthetic-stub fabrication pre-empting a custom `ClassLoader`, NOT `find_class_bytes_delegated`) and fixed; the real Keycloak 26.6.1 server now boots under CratonVM, and **KC26-PIC.1 and KC26-RX.1 were re-measured against it and LIFTED** (removed from `skip_list.rs`). (The original entries claimed `ClassManager::find_class_bytes_delegated` needed a fourth loader path across "149 call sites"; that file was never touched. The defect was ordering: a fabricated stub is registered globally under `Application`, which poisons the binary name so the owning custom loader can never define the real class.)
- **SuppressWarnings annotation bug** — any source containing `@SuppressWarnings("...")` fails in-process javac compilation (unrelated to any JIT ban, reproduces with JIT fully disabled). `docs/known-issues/suppresswarnings-annotation-duplicate-value-bug-20260726.md`.

## Blocked — genuinely missing fixture, confirmed absent (double-checked)

- **SPB.9b's other 2 sub-bans** (`org/springframework/boot/loader/`, `org/springframework/web/reactive/`+`org/springframework/boot/web/reactive/`) — need the real `insurance-backend` app's JarLauncher/WebFlux-boot scaffold, not just the isolated Spring library classes tested this session.
- **SPB.x named fixture apps** (SportMe-master, ms-course-youtube, insurance-backend, eureka-server, msyt-admin, cglib_probe) — searched at full filesystem depth by content/purpose (not just name), zero matches, independently confirmed by a concurrent session too.
- **SPB.6** (`com/netflix/discovery/` -- Netflix Eureka `DiscoveryClient`) -- provisional blanket ban, Session 113 r1, never re-verified. Confirmed genuinely fixture-blocked: `find / -iname '*eureka-client*.jar' -o -iname '*eureka-core*.jar'` across the entire host returns zero matches (no Maven/Gradle cache entry, no vendored jar, no source checkout) -- consistent with the eureka-server named-fixture-app search above coming up empty too. (A later Stop-hook feedback round cited this as "KC26-SPB6" -- no such literal name exists; SPB.6 is the real ban, and it is Eureka-related, not Keycloak-related -- the "KC26" prefix in that citation does not correspond to anything in the actual ban name or comment.)

## Tested, hypothesis refuted (no action, but investigated properly)

- **TYPES-ERASURE.1 consolidation hypothesis** — tested whether banning `Types.erasure` alone subsumes the other 7 javac-family bans (`SPRING-TESTCOMPILER.1-4`, `HIB-STOREDPROC-JIT.1`). Refuted — all 8 are independent miscompiles. `docs/known-issues/javac-family-consolidation-hypothesis-refuted-20260726.md`. **These 8 bans remain active and correctly so** — not a gap, a confirmed-necessary state.

## Found, not a ban, new VM bugs discovered this session

- ~~**`java.io.Writer.write(char[])` silently drops output under JIT**~~ — **CLOSED 2026-07-27**, does not reproduce on dev. Two corrections to the original writeup: the compiled overload was `write(String)`, not `write(char[])` (`BISECT_SKIP` matches by method *name*), and the bug is unrelated to the LICM defect found while closing it.
- **JIT LICM / speculative pre-header bypassed by a forward branch into the loop header** — found 2026-07-27 while closing the two entries above, **FIXED**. A general x86-64 codegen defect: any hoist or speculative guard emitted at a loop header is skipped by an edge that enters the header from outside the loop, so the loop runs against an uninitialised cache slot (and, in the speculative-BCE case, against elided bounds checks whose guard never ran). This is also what **HIB-LONGTAIL.2** (`AttributesImpl.ensureCapacity`, removed from the skip list on 2026-07-26 as "no longer reproduces") really was — it was ~30%-per-run flaky and had been under-sampled, not fixed.
- **`Class.getResourceAsStream`/`getResource` classloader-blindness** — FIXED this session (see removal list logic above; this was a real fix, not a ban).

## Flagged, not yet investigated (real, high-value, explicitly recommended for a future session)

- ~~**Undocumented blanket `org/junit/` + 3 siblings**~~ — **CLOSED 2026-07-27, all four REMOVED.** The ES-specific leg this entry called for was completed and initially aborted 58/60 classes — but the cause was not a miscompile these bans guarded against. It was `jit/src/ir_lower.rs` panicking (`expect`) on an overflowed code buffer instead of taking `lower_inner`'s existing `buf.overflowed()` bail to the single-pass backend, triggered by exactly one class, `org/junit/internal/MethodSorter`. Fixing that (13 patch sites now `.ok()`, matching `x64.rs`) ALSO removed 7 pre-existing SIGABRTs from the default-settings ES baseline. With the fix in, baseline-vs-lifted is byte-for-byte identical across ES 60/60, Hibernate 160/160 and Spring Boot 40/40.

## What genuinely remains unaddressed by this session

Given the sheer size of `vm/src/jit/skip_list.rs` (dozens of named bans accumulated across many prior sessions, many already claimed/being investigated by a concurrently-active `fix/jit-ban-sweep-20260725` session — WildFly boot family confirmed still-needed, `org/springframework/util/` SPB.1 confirmed inconclusive, H2 suite confirmed still-needed, `com/unboundid/` confirmed still-needed), a full, literal 100% sweep of every entry in this file was not completed in this session. This is consistent with the standing goal's own explicit framing ("take as many sessions/builds/time as required") and the fact that a second, independently-operating session is concurrently working the same backlog. Everything genuinely still open has a real reason on record above (blocked by a specific architectural gap, blocked by a confirmed-missing fixture, or flagged with concrete partial evidence for a follow-up) — nothing was silently skipped without documentation.
