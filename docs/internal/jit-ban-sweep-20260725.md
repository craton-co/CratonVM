# JIT ban sweep — 2026-07-25/26

Tracking doc for the goal "safely remove all app-specific JIT bans now that
the big JIT rework has landed; try every app with JIT and fix newly
discovered bugs." Source of truth for what's banned: `vm/src/jit/skip_list.rs`.
This file lives at `docs/internal/jit-ban-sweep-20260725.md` once committed;
this scratchpad copy is the working draft.

Worktree: `/data/data/wt-jitban-20260725`, branch `fix/jit-ban-sweep-20260725`.
Binaries: `/data/tmp/jitban-bins/cratonvm-jitban-<label>-<date>` (unique names
per the standing workflow, never reuse `target/release/cratonvm` directly for
timed runs — it gets clobbered by concurrent rebuilds).

**IMPORTANT — lane coordination:** another concurrent session on this host
owns `org/junit/runners/model/TestClass.collectAnnotatedMethodValues`
(TOMCAT-DOHEAD-JUNIT-ITERATOR.1, skip_list.rs ~L792). Do not touch that entry
from this branch.

## How the file is structured

- Lines ~490-915: targeted bans that apply under **both** `SkipPolicy::Conservative`
  and `SkipPolicy::Aggressive` (i.e. NOT liftable by flipping the policy —
  only by `CRATONVM_JIT_ALLOW_PACKAGES=<prefix>` matching the exact ban
  prefix string, or by editing the code).
- Lines ~920-2001: wrapped in `if policy == SkipPolicy::Conservative { ... }`
  — this is almost the entire per-framework blanket-ban list (springframework,
  jboss/wildfly, hibernate helpers, keycloak, reactivex, bouncycastle non-hotpath,
  intpoly, bytebuddy, log4j, junit-the-runner-itself, slf4j/logback/commons-logging,
  cglib, hsqldb, eclipse-jdt, hamcrest, minidev-json, unboundid-ldap, netflix,
  feign, elasticsearch(!), snakeyaml, sun/java beans...). **`jit_aggressive_compilation`
  has NO CLI/env wiring today** (only a `VmConfig` struct field, set `false` by
  default, flipped only in Rust unit tests) — so exercising this path from a
  suite run requires either adding CLI plumbing (`--jit-aggressive` flag or a
  `CRATONVM_JIT_AGGRESSIVE=1` env check in vm-cli) or testing package-by-package
  via the already-wired `CRATONVM_JIT_ALLOW_PACKAGES` env var. Went with the
  latter to avoid code-risk before any verification.
- Every ban site's comment documents its own `CRATONVM_JIT_ALLOW_PACKAGES=`
  lift recipe — use it verbatim to test.

## Host gotchas hit this session

- Root filesystem (`/`, `/tmp`) was **100% full (29G/29G, 0 avail)** —
  `cc`/rustc intermediate `.s` files failed with "No space left on device",
  masquerading as a build failure. Fix: `TMPDIR=/data/tmp` for cargo builds
  (`/data` has 143G free on nvme1n1). Also affects **test working directories**
  — H2 test classes write relative `./data/...` — must `cd` into a `/data/tmp/...`
  workdir before invoking the VM, or they hit the same ENOSPC.
- `cargo` isn't on PATH for non-interactive SSH — `source ~/.cargo/env` first.
- H2 test classes are their own JUnit-less main() entry points
  (`TestBase.createCaller().init().testFromMain()`) — invoke the class
  directly as the main class, NOT via `org.junit.runner.JUnitCore`.
- H2 classpath = `$H2_ROOT/target/classes:$H2_ROOT/target/test-classes:$(cat $H2_ROOT/craton-testcp.txt)`
  (`H2_ROOT=/data/data/h2database/h2`); the bare `craton-testcp.txt` alone
  is missing the H2 classes themselves.

## Status legend

TODO = not yet tested · TESTING = build/run in flight · KEPT = tested, still
needed (root cause not yet fixed, or newly-discovered bug found and fixed but
ban intentionally narrowed rather than removed) · REMOVED = ban deleted from
skip_list.rs, verified clean, committed.

## Targeted bans (apply under BOTH policies — need explicit per-prefix testing)

| Tag | ~Line | Prefix/Class.method | Notes | Status |
|---|---|---|---|---|
| (generic) | 494 | `java/util/Collections.indexedBinarySearch` | ES812 lambda-receiver residual | TODO (not app-specific, skip) |
| (generic) | 507 | `java/util/stream/MatchOps.{makeInt,makeRef,makeLong,makeDouble}` | ES-PERF uncommon-trap | TODO (not app-specific, skip) |
| SPRING-TESTCOMPILER.1 | 526 | `com/sun/tools/javac/api/JavacTool.getTask` | in-process javac miscompile | TODO |
| SPRING-TESTCOMPILER.2 | 561 | `com/sun/tools/javac/jvm/ClassReader.readClass` | ditto | TODO |
| SPRING-TESTCOMPILER.3 | 595,599 | `ClassFinder.{complete,fillIn}` | ditto | TODO |
| (javac family) | 603,607 | `ClassReader.{readInnerClasses,readAttrs}` | ditto | TODO |
| HIB-STOREDPROC-JIT.1 | 620 | `Symbol$ClassSymbol.complete` | H2 CREATE ALIAS javac | TODO |
| SPRING-TESTCOMPILER.4 | 629 | `org/springframework/javapoet/CodeBlock$Builder.add` | shaded javapoet | TODO |
| SPRINGBOOT-WITHOUT-JACKSON.2 | 648 | `ModifiedClassPathClassLoader.loadClass` | boot testsupport | TODO |
| (generic) | 740 | `java/math/MutableBigInteger` (whole class) | Hibernate AIOOBE | TODO (VM-wide, low priority) |
| ANTLR-COLDPATH.1 | 751 | PredictionContext cluster (fn `is_antlr_prediction_context_miscompile`) | stays banned even w/ groovyjarjarantlr4/ allowed | KEPT by design |
| PROXY-JITCALL.1 | 763 | `$ProxyN` dynamic-proxy classes (fn `is_generated_proxy_class`) | JIT-JIT call boundary register bug | TODO — good rework-relevance candidate |
| SPR-AOT-TESTNG-MAPS.1 | 784 | `org/testng/collections/Maps` | TestNG helper | TODO |
| TOMCAT-DOHEAD-JUNIT-ITERATOR.1 | 792 | `TestClass.collectAnnotatedMethodValues` | **OWNED BY ANOTHER SESSION — DO NOT TOUCH** | SKIP (lane conflict) |
| REACTOR-ADDCAP.1 | 814 | `reactor/core/publisher/Operators.addCap` | demand accounting | TODO |
| REACTOR-FLUXCREATE.1 | 822,825 | `FluxCreate$BaseSink.addCap`, `FluxCreate$BufferAsyncSink.drain` | ditto | TODO |
| JETTY-WSIO.1 | 836 | `org/eclipse/jetty/{websocket,io}/` | needs both compiled together to repro | TODO |
| **HIB-LONGTAIL.1** | 871 | `org/h2/`, `org/antlr/v4/runtime/` | **IN PROGRESS THIS SESSION** — perf-motivated, not correctness | TESTING |
| HIB-LONGTAIL.2 | 882 | `org/xml/sax/helpers/AttributesImpl.ensureCapacity` | anewarray corruption | TODO |
| HIB-LONGTAIL.3 | 892 | `GenerationTargetToScript.<init>` | uninit field | TODO |

## Conservative-only block (L920-2001) — liftable per-prefix via CRATONVM_JIT_ALLOW_PACKAGES

Not yet individually tested this session. Full prefix list (from grep, see
skip_list.rs L935-1990): eclipse-jdt parser+ast, elasticsearch(!), hamcrest,
wildfly-controller, minidev-json, unboundid-ldap, keycloak/picocli/smallrye,
rxjava3, bouncycastle (non-hotpath), intpoly, bytebuddy, randomizedtesting,
log4j, junit/+junit-the-jar, springframework.{util,core,boot.context.properties.bind,
boot.context,boot(!),cloud}, groovyjarjarantlr4, antlr-runtime(again, redundant
with HIB-LONGTAIL.1?), netflix-discovery, feign, jboss.{modules,as},
wildfly, jboss.msc, jboss.logging, slf4j, logback, commons-logging, cglib,
hsqldb, springframework.boot.loader, spring-web-reactive, spring-boot-web-reactive,
spring-beans-factory-support, spring-context-annotation, spring-context-support,
spring-core-io-support, sun-beans, java-beans, spring-beans-factory,
junit-platform-console-picocli.

**Not yet catalogued individually — next session(s) should read L920-2001
in full and expand this table before testing.**

## Next steps

1. Finish H2/ANTLR-runtime test (this session) — compare `TestFileSystem`
   wall-clock lifted vs baseline, then run full 218-class suite if promising.
2. Expand the Conservative-block table above with exact line numbers.
3. Work app-by-app: H2 (this session) → Tomcat (JASPER-JDT.2/3, jetty-wsio,
   javac family via Hibernate-in-Tomcat paths) → Spring Boot (huge chunk of
   springframework.* + javac family + javapoet) → Keycloak (KC26-PIC,
   KC26-RX, intpoly, smallrye/picocli) → Elasticsearch (whole-package
   Conservative ban + hamcrest + randomizedtesting) → WildFly (jboss.*,
   wildfly-controller) → Kafka.
4. For each verified-safe removal: delete the ban's code + its doc comment,
   update/retire any `docs/known-issues/*` or `docs/internal/*` doc that
   referenced it as open, commit on this branch, periodically merge to `dev`
   and push (don't wait for 100% completion — batch verified wins).
