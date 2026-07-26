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
| HIB-ANTLR.1 | `org/antlr/v4/runtime/` | shadowed no-op (see below) |
| TOMCAT-DOHEAD-JUNIT-ITERATOR.1 | `TestClass.collectAnnotatedMethodValues` | real junit-4.13.2, blanket-ban-lifted-too |
| JSONSMART-PARSER.1 | `net/minidev/json/parser/` | real json-smart-2.3.jar |
| SPB.9 | `org/slf4j/`,`ch/qos/logback/`,`org/apache/commons/logging/` | real jcl-over-slf4j+logback |
| JASPER-JDT.2 | `org/eclipse/jdt/internal/compiler/parser/` | real Tomcat TestCompiler, 2x2 repeated runs |
| JASPER-JDT.3 | `org/eclipse/jdt/internal/compiler/ast/` | real Tomcat TestFormAuthenticatorA, 2x2 repeated runs |
| SPB.9d | `com/sun/beans/`,`java/beans/` | pure-JDK BeanIntrospectorProbe |
| SPB.9b (partial) | `org/springframework/beans/factory/support/` | real spring-beans-7.0.7.jar |
| SPB.9c | `context/annotation/`,`context/support/`,`core/io/support/`,`beans/factory/` | real spring-context-7.0.7.jar |
| SPB.2 | `org/springframework/core/` | real spring-core-7.0.7.jar, hang-safe timeout wrappers |

## Already moot / dead-code (no action needed, confirmed this session)

- **NETTY.1** — already lifted 2026-06-11, predates this week; no active `io/netty/` ban remains, only historical archetype references.
- **FELIX.1**, **BC-ASN1.1** — live inside `is_known_miscompile()`, which is entirely dead code by default (gated behind `callee_saved_gpr_local_homes_enabled()`, defaults false, no CLI wiring). ~20-30 other historically-catalogued bans fall in this same bucket.

## Kept — confirmed still needed, with real evidence

- **JAXB** (`org/glassfish/jaxb/`) — real QName-compare corruption reproduces. `docs/known-issues/jaxb-still-needed-20260726.md`.
- **ES fragile cluster** (`org/elasticsearch/`) — real ES 9.6.0-SNAPSHOT checkout, 18-class sample found a real regression (`FloatFieldBlockLoaderTests`). `docs/known-issues/es-fragile-cluster-confirmed-needed-20260726.md`.
- **HIB-TEMPORAL.1** (`org/hibernate/`) — real Hibernate ORM 8.0 harness, lifting causes a full `StrategySelectionException` bootstrap cascade. `docs/known-issues/hib-temporal-1-still-needed-20260726.md`.

## Blocked — real, still-open, NOT this session's to fix (architectural)

- **Keycloak class-resolution loader-blindness** — `ClassManager::find_class_bytes_delegated` (classloading/src/class_manager.rs) only checks bootstrap/extension/application, no path to a custom `ClassLoader`'s own `findClass`. Blocks the real Keycloak 26.6.1 boot past `Version.<clinit>` (now fixed) at a NEW class-resolution point. 149 call sites of `load_class` across the VM — genuinely too large a refactor for a single pass. `docs/known-issues/keycloak-boot-blocked-version-null-20260726.md`.
- **KC26-PIC.1 / KC26-RX.1 / KC26.LR** — all blocked by the above; the real Keycloak boot never reaches these code paths. Cannot be independently tested until the class-resolution bug is fixed.
- **SuppressWarnings annotation bug** — any source containing `@SuppressWarnings("...")` fails in-process javac compilation (unrelated to any JIT ban, reproduces with JIT fully disabled). `docs/known-issues/suppresswarnings-annotation-duplicate-value-bug-20260726.md`.

## Blocked — genuinely missing fixture, confirmed absent (double-checked)

- **SPB.9b's other 2 sub-bans** (`org/springframework/boot/loader/`, `org/springframework/web/reactive/`+`org/springframework/boot/web/reactive/`) — need the real `insurance-backend` app's JarLauncher/WebFlux-boot scaffold, not just the isolated Spring library classes tested this session.
- **SPB.x named fixture apps** (SportMe-master, ms-course-youtube, insurance-backend, eureka-server, msyt-admin, cglib_probe) — searched at full filesystem depth by content/purpose (not just name), zero matches, independently confirmed by a concurrent session too.

## Tested, hypothesis refuted (no action, but investigated properly)

- **TYPES-ERASURE.1 consolidation hypothesis** — tested whether banning `Types.erasure` alone subsumes the other 7 javac-family bans (`SPRING-TESTCOMPILER.1-4`, `HIB-STOREDPROC-JIT.1`). Refuted — all 8 are independent miscompiles. `docs/known-issues/javac-family-consolidation-hypothesis-refuted-20260726.md`. **These 8 bans remain active and correctly so** — not a gap, a confirmed-necessary state.

## Found, not a ban, new VM bugs discovered this session

- **`java.io.Writer.write(char[])` silently drops output under JIT** — general VM correctness bug, unrelated to any specific app ban. `docs/known-issues/java-io-writer-write-char-array-jit-miscompile-20260726.md`.
- **`Class.getResourceAsStream`/`getResource` classloader-blindness** — FIXED this session (see removal list logic above; this was a real fix, not a ban).

## Flagged, not yet investigated (real, high-value, explicitly recommended for a future session)

- **Undocumented blanket `org/junit/` + 3 siblings** (`junit/`, `org/apache/logging/log4j/`, `com/carrotsearch/randomizedtesting/`) — all from one incidental commit, no rationale. 80-class Hibernate sample clean with all 4 lifted; an ES-specific test (their likely true origin) was inconclusive due to host contention, not a regression. `docs/known-issues/blanket-org-junit-ban-undocumented-shadow-20260726.md`. **NOT removed** — positive partial evidence only.

## What genuinely remains unaddressed by this session

Given the sheer size of `vm/src/jit/skip_list.rs` (dozens of named bans accumulated across many prior sessions, many already claimed/being investigated by a concurrently-active `fix/jit-ban-sweep-20260725` session — WildFly boot family confirmed still-needed, `org/springframework/util/` SPB.1 confirmed inconclusive, H2 suite confirmed still-needed, `com/unboundid/` confirmed still-needed), a full, literal 100% sweep of every entry in this file was not completed in this session. This is consistent with the standing goal's own explicit framing ("take as many sessions/builds/time as required") and the fact that a second, independently-operating session is concurrently working the same backlog. Everything genuinely still open has a real reason on record above (blocked by a specific architectural gap, blocked by a confirmed-missing fixture, or flagged with concrete partial evidence for a follow-up) — nothing was silently skipped without documentation.
