# JIT skip-list open-ban survey (2026-07-25/26)

Working backlog for the "remove the JIT ban, fix the residuals" effort.
`vm/src/jit/skip_list.rs` currently carries **~46 distinct named
correctness bans** plus the four structural ones (`<clinit>`, `<init>`,
interface defaults, `java/util/`+`cratonvm/` conservative package bans).
This doc catalogs what's known about each cluster as of this session so
follow-up work doesn't have to re-derive it from scratch.

## Methodology that actually works here (use this first)

Three runtime env vars let you test a ban hypothesis **against an
already-built binary, no rebuild required**:

- `CRATONVM_JIT_DENY=<substring>` — comma-separated substrings matched
  against `Class.method`; any match is forced to interpret. Good for
  coarse package-level bisection (`CRATONVM_JIT_DENY=com/sun/tools/javac/`).
- `CRATONVM_JIT_BISECT_SKIP=<Class.method>,...` — exact `Class.method`
  pairs (slash-separated class names) forced to interpret. Good for
  narrowing to one method once `_DENY` has isolated a package/class.
- `CRATONVM_JIT_BISECT_ONLY=<prefix>,...` — inverse: only listed package
  prefixes stay JIT-eligible, everything else interpreted.
- `CRATONVM_JIT_ALLOW_PACKAGES=<prefix>,...` — lifts the *conservative*
  package bans (`java/util/`, `cratonvm/`, and several per-cluster ones
  gated the same way) for benchmarking/dev; does NOT lift the unconditional
  targeted bans (those need a source edit + rebuild).

Binary search via `_DENY` (whole package → half → quarter → single method)
is how every cluster below with a "bisected to X" note was actually found.
It costs zero rebuild time and should be the first move on any of the
still-OPEN items.

## This session's concrete finding: TYPES-ERASURE.1 (NEW, landed)

`com.sun.tools.javac.code.Types.erasure` was an **undiscovered** JIT
miscompile in the same "repeated in-process javac compilation" family as
`SPRING-TESTCOMPILER.1-4` / `HIB-STOREDPROC-JIT.1` (7 existing bans, all in
`com/sun/tools/javac/{jvm,code}/*`, all dated 2026-07-18 through 07-23).
None of those 7 existing bans prevented this new crash. Minimal 13-line
standalone repro (no Spring, no Tomcat, no annotation processing — just
`ToolProvider.getSystemJavaCompiler().getTask(...).call()` looped ~40x,
each iteration compiling a fresh trivial `@Deprecated` class): fails
deterministically from iteration 8 onward with
`NullPointerException: ... "type" is null` from javac's own
`Lower.boxIfNeeded`. Bisected (via `CRATONVM_JIT_DENY`, no rebuild) to a
single method: `Types.erasure`. Added as `TYPES-ERASURE.1` /
`SkipReason::TypesErasure`, verified 40/40 pass with just this one method
denied.

**Follow-up not yet done:** `Types.erasure` is called constantly during
symbol/type completion, including from the other 7 already-banned methods
in this family. It is plausible (not yet verified) that fixing/banning
`erasure` alone makes those 7 bans redundant — i.e. this one fix might let
you *remove* 7 other entries, not just add an 8th. Verifying that requires
a full regression pass with those 7 bans temporarily lifted and only
`TypesErasure` in place. This is the single highest-leverage next step in
the whole skip list: a working, cheap (no-rebuild) bisection method already
exists, the repro is 13 lines, and the payoff is collapsing 8 bans into
(at best) 1 real fix instead of 8 separate whack-a-mole entries.

## TYPES-ERASURE.1 consolidation hypothesis — REFUTED 2026-07-26 02:30 UTC

Tested by temporarily gating all 6 other javac-family bans
(`ClassReader.readClass`, `ClassFinder.complete`, `ClassFinder.fillIn`,
`ClassReader.readInnerClasses`, `ClassReader.readAttrs`,
`Symbol$ClassSymbol.complete`) behind a test-only env var
(`CRATONVM_JIT_TESTONLY_UNBAN_SIX`, never committed) while leaving
`Types.erasure`'s ban (TYPES-ERASURE.1) in place, then running a stress
repro (`JavacLoopProbe2.java` — 100 iterations, each compiling a class with
a `@Deprecated` member, a JSpecify `@Nullable` TYPE_USE-annotated generic
method return + array return, and a generic method, matching the union of
all 7 original bans' trigger shapes).

**Result: still fails.** With all 6 lifted (only `Types.erasure` banned):
continuous failures (16 ok / 45 fail by iteration 60) with a THIRD, distinct
crash signature not previously catalogued:
`NullPointerException: Cannot read field "kind" because "tree.sym" is null`
from `com.sun.tools.javac.comp.Flow$CaptureAnalyzer.visitIdent`. Confirmed
JIT-specific: the identical probe under `--nojit` is 30/30 clean (0
failures).

Bisecting which of the 6 is load-bearing (re-pin one via
`CRATONVM_JIT_DENY=<Class.method>` while the other 5 stay lifted, no
rebuild needed): `ClassReader.readClass` alone insufficient (still fails
continuously). `ClassFinder.complete` alone: failures cluster ONLY in
iterations 6-16 (11 failures) then completely stop for the remaining 83
iterations — a partial improvement, but not clean, meaning `complete`
contributes but at least one more of the remaining 5
(`fillIn`/`readInnerClasses`/`readAttrs`/`ClassSymbol.complete`) or a
genuinely new corruptor is also still needed. Did not finish narrowing
further this session (time-boxed).

**Conclusion: the consolidation hypothesis is false as a blanket claim.**
`Types.erasure` is real and independently worth banning (see
TYPES-ERASURE.1 above) but does NOT subsume the other 6 javac-family bans.
All 6 must stay. Do not re-attempt this exact consolidation without new
evidence; if picked up again, the `CRATONVM_JIT_TESTONLY_UNBAN_SIX`-style
gate + `CRATONVM_JIT_DENY` bisection (both no-rebuild after the first gated
build) is a fast way to keep narrowing which subset is load-bearing --
`ClassFinder.complete` is the next lead, not yet fully isolated from the
remaining 4.

## TOMCAT-DOHEAD-JUNIT-ITERATOR.1 — investigated, inconclusive

2026-07-22 ban on `org/junit/runners/model/TestClass
.collectAnnotatedMethodValues` (enhanced-for iterator local allegedly goes
null under JIT, mid heavy-allocation reflective invoke). Two standalone
repros built this session, both stressing exactly this bytecode shape
(iterator held live across a reflective `Method.invoke`/`invokeExplosively`
call that allocates heavily to force GC — 500k iterations each):

- `IterReflProbe.java` — raw `Method.invoke`, no JUnit involved: clean pass.
- `DirectIterProbe.java` — calls the actual banned method directly
  (`TestClass.collectAnnotatedMethodValues` via real JUnit 4.13.2, 8
  `@Rule`-annotated methods, 500k calls) **with the ban source-patched out**
  under full JIT: clean pass (`DONE n=500000`), no NPE.

This is decent evidence the underlying bug may be stale (fixed by one of
the many general GC-root-tracking fixes landed since 2026-07-22 — several
memory-tracked fixes in that exact bug class landed this week). **Not
conclusive**: attempts to reproduce with the *original* Tomcat DoHead
JUnit-suite test (the doc's own repro) hit a flaky, reproduces-with-or-
without-the-ban `internal error: current class not found` — almost
certainly host contention noise (this is a heavily shared 16-core Azure
box with a dozen+ concurrent worktrees), not signal. Recommend: rerun the
full DoHead matrix on a quiet host window before touching this ban for
real; don't trust a single noisy run either way.

## Already resolved / no action needed (confirmed this session or by prior commits already on dev)

- `org/bouncycastle/` (BC-JIT family) — **MUST STAY, permanently.**
  Thoroughly root-caused in `docs/internal/gaps/bc-jit-miscompile-handoff.md`:
  the one real miscompile (OSR loop-overrun AIOOBE via an `ldc`
  bytecode-length bug) is fixed on dev; the remaining blocker is a genuine
  throughput wall (SPHINCS-256 needs near-HotSpot crypto throughput the JIT
  doesn't deliver), not a further correctness bug. Do not re-litigate
  without new evidence.
- `java/lang/*`, `cratonvm/Tck*`, `FinalizerTest` — REMOVED already (NEW-1.2,
  NEW-1.5), kept only as dead enum variants for ABI/log compatibility.
- `LUCENE-POSTINGS.1` — the original blanket `org/apache/lucene/*` ban was
  retired (IndexWriter monitor fix), but a 2026-07-25 re-verification
  commit (`cfbb315e6`, `393a31579`) says "ban stays" for *something* in
  this area post-MatchOps fixes — the exact current scope wasn't re-derived
  this session; read those two commits before assuming either state.

## Not investigated this session — open backlog, roughly prioritized

High-value (wide blast radius or already well-isolated in comments):

- **SPB.1/.2/.4/.4b/.4c/.5/.6/.7/.8/.8b/.8c/.9/.9b/.9c/.9d, RBC.1, CGL.1,
  PIC.1, W2-CHM, EXEC.1** — ~20 Spring-Boot-era blanket package bans, all
  explicitly cross-referenced in their own comments as "the same
  allocate-then-putfield archetype" (a hypothesized JIT bug: a freshly
  allocated object's field stores, written immediately after `new`, get
  corrupted/miscompiled, esp. across a GC-triggering call). **Caution:**
  a fresh, unrelated 2026-07-25 finding
  ([[jit-inline-tlab-header-before-cursor-commit]] memory /
  `gc/src/gen_heap.rs`) confirms the *allocator's own header writes* are
  correct and linearized properly — so if this bug is still real, it is in
  the JIT's `putfield` codegen or register allocation *after* `new`
  returns, not in the allocator. Worth a fresh bisection pass (same
  `CRATONVM_JIT_DENY` technique) on ONE of these (e.g. SPB.1,
  `org/springframework/util/`, has a documented Spring Boot repro) before
  assuming the theory is even still correct — it may also be stale, like
  TOMCAT-DOHEAD-JUNIT-ITERATOR.1 turned out to possibly be.
- **ANTLR.1 / ANTLR-COLDPATH.1 / HIB-ANTLR.1** — shaded ANTLR v4 runtime,
  used by Groovy/Hibernate/Keycloak; comment already narrows suspicion to
  7 specific ATN config-context methods but keeps a package-level ban as
  the "surgical per-method ban of those 7 is the future minimal fix."
  Doing that narrowing is concrete, scoped work.
- **KC26-PIC.1/.2, KC26-CFG.1, KC26-RX.1** — Keycloak/picocli/smallrye/
  RxJava3 hangs, already narrowed once (picocli itself un-banned, a
  specific interceptor fan-out class stays banned). Testable via the
  Keycloak suite (`apps/keycloak`).
- **JUNIT.1** — generic `JUnitCore.main` ban; if liftable, improves JIT
  coverage across every suite's test-running machinery, not just one
  class. Comment notes a `DBG bypass` already exists to force-compile it
  for diagnosis — someone already built the tooling, never finished the
  investigation.

Medium (single-suite, already narrowly scoped, lower blast radius but
still real correctness bugs worth closing):

- HIB-TEMPORAL.1, HIB-LONGTAIL.1/2/3, HIB-BIGINTEGER-AIOOBE.1,
  HIB-STOREDPROC-JIT.1 (candidate for TYPES-ERASURE.1 consolidation, see
  above)
- JAXB (`jaxb_mapping_residual_skip_prefix`), Xerces
  (`xerces_schema_jit_deny_prefix`), SnakeYAML emitter
- ES-HAMCREST.1, ES-JIT-DEOPT-GC.1, ES fragile cluster
  (`is_elasticsearch_suite_jit_fragile_cluster`)
- JSONSMART-PARSER.1, JASPER-JDT.2/.3, WILDFLY-CONTROLLER-JIT.1
- TOMCAT-JNDIREALM-RDN.1, TOMCAT-JNDIREALM-JIT.2, PROXY-JITCALL.1,
  SPR-AOT-TESTNG-MAPS.1
- REACTOR-ADDCAP.1, REACTOR-FLUXCREATE.1, JETTY-WSIO.1, NETTY.1 — all
  Reactor/Jetty/Netty websocket demand-accounting bugs, share a "JIT
  long/CAS lowering bug" hypothesis across three separate entries; another
  case where one root cause may explain all three.
- FELIX.1, BC-ASN1.1, SPB-FLYWAY-HSQLDB.1, SPRINGBOOT-WITHOUT-JACKSON.2 —
  single-cluster, not yet re-examined.
- AQS/RRWL family (`is_known_miscompile_aqs_family`), CLQ family
  (`is_known_miscompile_clq_family`) — large, well-tested, tied into the
  separate H2 testConcurrent perf investigation (see
  `docs/internal/gaps/h2-*testconcurrent*` and memory
  `h2-suite-fail-triage`/`h2-testfilesystem-testconcurrent`). Not
  re-verified this session; do not touch without reading that context
  first, these interact with a live perf investigation.

## Recommended next session priority

**Status (this session, branch `fix/jit-ban-sweep-20260725`):**
- Item 1 — **DONE, REFUTED 2026-07-26 02:30 UTC** (see consolidation-hypothesis section above).
- Item 2 (SPB.1 bisection) — **SKIPPED, owned by the other concurrent
  session** (`fix/jit-ban-sweep2-20260726` / `wt-jitsweep2-20260726`, see
  `docs/internal/jit-ban-sweep-20260725.md` — already actively testing the
  SPB/CGL/PIC/WildFly family). Do not duplicate.
- PROXY-JITCALL.1 — REMOVED (see above, merged dev@07427a14e).

**Status (`fix/jit-ban-sweep2-20260726` / `wt-jitsweep2-20260726`):**
- Item 2 (SPB.1 bisection, `org/springframework/util/`) — **DONE (started
  2026-07-26 02:20 UTC, concluded 02:24 UTC): INCONCLUSIVE, ban KEPT.**
  Three standalone repros against a real `spring-core-7.0.7.jar` (no
  fixture app available on host): two clean-load variants (with/without
  HashMap-machinery warmup) passed identically in both baseline and lifted
  configs; a third, more aggressive GC-pressure + classloader-churn variant
  crashed **both** configs (differently — baseline hit
  `gen_heap::read_slot: corrupt Value cell` heap corruption landing in
  `ClassUtils.registerCommonClasses` with an NPE, matching the *original*
  bug's crash site but under the config that should be protected from it;
  lifted hit a `ClassCastException` reading back a corrupted field). Since
  baseline (ban active) also crashed, this can't be cleanly attributed to
  lifting *this* ban — likely either a separate GC-root/classloader-churn
  bug my repro's own design exercises, or a timing-sensitive race. Full
  writeup + all 3 repros: `docs/known-issues/spb1-springframework-util-investigation.md`,
  `docs/known-issues/repros/spb1-classutils/`. **Verdict: KEEP the ban** (no
  positive evidence to remove); the repro-3 crash is flagged as a separate,
  possibly-serious open issue for whoever wants to chase it, independent of
  SPB.1. Confirms this session's now-established pattern: `org/jboss/as/`
  and `org/h2/` were BOTH confirmed still-needed via real-app testing;
  synthetic repros for this ban family have proven unreliable/hard to
  construct faithfully — prefer a real app/suite over a hand-rolled probe
  when one is available for any future items in this family.

1. Verify (or refute) the TYPES-ERASURE.1 consolidation hypothesis — highest
   expected value, cheapest to test (no-rebuild env-var bisection already
   proven to work on this exact cluster).
2. Fresh `CRATONVM_JIT_DENY` bisection on ONE SPB.x ban (suggest SPB.1,
   `org/springframework/util/`, has the clearest documented repro) to
   check whether the "allocate-then-putfield" theory still holds post the
   2026-07-25 TLAB-header finding, or whether it's stale like the JUnit
   iterator ban may be.
3. ANTLR.1 narrowing (7 specific methods already named in the comment).
   **CLAIMED by `fix/jit-ban-sweep2-20260726` / `wt-jitsweep2-20260726`,
   2026-07-26 ~02:30 UTC.** Note before starting: the "narrowing" is
   actually already done at the code level — `is_antlr_prediction_context_miscompile`
   (skip_list.rs ~L3199, the exact 7 methods this item names) is already an
   *unconditional* guard, active regardless of policy/`CRATONVM_JIT_ALLOW_PACKAGES`
   (see ANTLR-COLDPATH.1's comment, ~L744: "stays interpreted even when
   `CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/` lifts the surrounding
   package"). So the *correctness* reason (reason 1 in ANTLR.1's own
   comment) is already independently covered. The real remaining question
   is whether the *throughput* reason (reason 2: "~8x slower cold parse
   under JIT") still holds post today's perf-focused JIT rework
   (compressed oops / bytecode quickening / IR call lowering) — if not,
   the broader `groovyjarjarantlr4/` blanket ban (Conservative-only) could
   be lifted while the narrow 7-method guard stays as the correctness
   safety net. Plan: find/build a Groovy parse benchmark, compare cold-parse
   wall-clock lifted vs. baseline.
4. Work down the "Medium" list — each is single-suite, well-scoped, lower
   risk of interacting with concurrent work elsewhere.

## TOMCAT-JNDIREALM-RDN.1 / JIT.2 — CLAIMED 2026-07-26 02:38 UTC

Picked from the Medium list, branch `fix/jit-ban-sweep-20260725`, since
SPB.1 and ANTLR.1 are now both owned by the other session
(`fix/jit-ban-sweep2-20260726`). UnboundID in-memory LDAP path
(`com/unboundid/ldap/sdk/RDN.getNameValuePairs` narrow guard +
`com/unboundid/` whole-package guard, skip_list.rs ~L1138-1177).
Testable via Tomcat's 76-case `TestJNDIRealmIntegration` matrix on this
host's Tomcat fixture (`/data/data/apps/tomcat`). Starting with the
narrow RDN.getNameValuePairs guard first.
