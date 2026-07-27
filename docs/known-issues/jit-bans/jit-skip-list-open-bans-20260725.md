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

**Shortcut found 2026-07-26 ~02:58 UTC (`fix/jit-ban-sweep2-20260726`):
skip the whole `is_known_miscompile` match-arm cluster.** Every entry
inside that function's `matches!(...)` block (HashMap.put/get/resize,
`Calendar.isFieldSet` (BC-ASN1.1), the ByteBuddy/reflection/JUC entries
catalogued in `jit-regalloc-callee-saved-clobber-family.md`, etc.) is only
even *checked* when `callee_saved_gpr_local_homes_enabled()` returns true
(skip_list.rs ~L1176: `if callee_saved_gpr_local_homes_enabled() &&
is_known_miscompile(...)`), and that function
(skip_list.rs ~L3253) defaults to **false** on x86_64 unless a developer
explicitly sets `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=1` for
diagnosis. **None of these entries are active in any normal run** (none of
this session's test commands set that var) — they're already effectively
"removed" from production's perspective, just kept as a diagnostic legacy
table. Don't spend real-app-testing effort re-verifying any ban that lives
inside `is_known_miscompile`'s `matches!` block specifically for this
reason (BC-ASN1.1 is one example — there may be others in that block worth
a quick grep before picking a next target).

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

- **SPB.2/.4/.4b/.4c/.5/.6/.7/.8/.8b/.8c/.9/.9b/.9c/.9d, RBC.1, CGL.1,
  PIC.1, W2-CHM, EXEC.1** — ~19 remaining Spring-Boot-era blanket package
  bans (SPB.1 itself, `org/springframework/util/`, was REMOVED 2026-07-26 —
  see the "UPDATE" note above and
  `docs/internal/fixed-suite-bugs/spb1-springframework-util-investigation-FIXED.md`;
  its "allocate-then-putfield" crash turned out to be a GC-root-scanning gap,
  not a JIT miscompile), all explicitly cross-referenced in their own
  comments as "the same allocate-then-putfield archetype" (a hypothesized JIT
  bug: a freshly allocated object's field stores, written immediately after
  `new`, get corrupted/miscompiled, esp. across a GC-triggering call).
  **Caution:** a fresh, unrelated 2026-07-25 finding
  ([[jit-inline-tlab-header-before-cursor-commit]] memory /
  `gc/src/gen_heap.rs`) confirms the *allocator's own header writes* are
  correct and linearized properly — so if this bug is still real, it is in
  the JIT's `putfield` codegen or register allocation *after* `new`
  returns, not in the allocator. **SPB.1's own resolution suggests checking
  the same GC-root gap first** (a still-young object owned by a
  user-defined-loader class, deferred to `metadata_pin` instead of being
  rooted directly — see `gc/src/vm_heap.rs::metadata_pin_deferrable`) before
  assuming a JIT miscompile for any of these remaining bans; several are
  themselves user-defined-loader-adjacent (CGL.1/cglib, PIC.1 likely a
  proxy/picocli generator). Worth a fresh bisection pass (same
  `CRATONVM_JIT_DENY` technique) on one of these before assuming the JIT
  theory is even still correct for them — it may also be stale, like
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
- JSONSMART-PARSER.1 — **DONE 2026-07-26 03:12 UTC: CONFIRMED still needed,
  ban KEPT.** Standalone stress repro (`docs/known-issues/repros/jsonsmart/JsonSmartProbe.java`,
  10 varied JSON docs × 300k iterations, round-trip parse/serialize/re-parse
  check) against `json-smart-2.6.0.jar`. Baseline (ban in place): 0 errors
  in whatever it completed within a 200s budget (interpreted parsing is
  slow, never finished one 30k-iter checkpoint). Lifted
  (`CRATONVM_JIT_ALLOW_PACKAGES=net/minidev/json/parser/`): **~20% error
  rate** (239,605/1,200,010 ops), corruption starting within the first ~5
  iterations and producing a DIFFERENT exception message for the identical
  input document across consecutive iterations (`"tab" at position 6` →
  `"tab":"a\tb at position 12` → `character (a) at position 5`, all for the
  same doc) — a live-state-dependent miscompile signature, not a
  deterministic parser bug. Full writeup:
  `docs/known-issues/jsonsmart-parser-still-needed.md`. Fourth-for-four
  real-app/faithful-repro confirmation this session that this ban family
  (`org/jboss/as/`, `org/h2/`, `com/unboundid/`, now this) is still fully
  live — nothing in it has been found safe to remove yet.
  JASPER-JDT.2/.3 (owned by other session, see below).
  WILDFLY-CONTROLLER-JIT.1 (`org/jboss/as/controller/`) — **already
  transitively confirmed still-needed**: it's a strict subset of the
  broader `org/jboss/as/` prefix this session already lifted for the
  `org/jboss/as/` WildFly-boot test (`docs/known-issues/wildfly/modeltypevalidator-validtypes-npe.md`)
  — `package_allowed()` lifts any ban whose prefix starts with an allowed
  entry, so lifting `org/jboss/as/` also lifted `org/jboss/as/controller/`
  in that same run. The crash found (`ModelTypeValidator.validTypes` null)
  is literally inside `org.jboss.as.controller.*`, the same symptom class
  (a field null after construction under JIT) as this ban's own
  `AbstractOperationContext.<init>`/`controllerOperations` null report —
  very likely the same underlying bug family, possibly the same bug. No
  separate test needed; KEEP.
- TOMCAT-JNDIREALM-RDN.1, TOMCAT-JNDIREALM-JIT.2 — **BOTH REMOVED
  2026-07-26, root-caused and fixed** (TOMCAT-JNDIREALM-JIT.3, the
  `string_case_cache` cross-thread root gap — see the section below).
  PROXY-JITCALL.1, SPR-AOT-TESTNG-MAPS.1
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
  writeup + all 3 repros (at the time): `docs/known-issues/repros/spb1-classutils/`.
  Confirms this session's now-established pattern: `org/jboss/as/`
  and `org/h2/` were BOTH confirmed still-needed via real-app testing;
  synthetic repros for this ban family have proven unreliable/hard to
  construct faithfully — prefer a real app/suite over a hand-rolled probe
  when one is available for any future items in this family.

  **UPDATE 2026-07-26 (later session): the repro-3 crash was chased down —
  ban REMOVED.** It was never a JIT bug: `vm/src/memory/roots.rs` had a
  GC-root-scanning gap (four sections deferred a still-young-generation
  object owned by a user-defined-loader class to the `metadata_pin`
  side-channel, which the Generational backend's old-gen-only mark BFS
  never covers for young objects — so a static field's value with no other
  root was silently reclaimed and its memory reused moments later by the
  class's own next allocation). Fixed via `VmHeap::metadata_pin_deferrable`
  (`gc/src/vm_heap.rs`) gating the defer on old-gen containment; same defect
  found and fixed in `native-builtins/src/phases_late.rs`'s `ClassValue`
  cache. Verified 50/50 clean with the ban kept and 47/47 clean with it
  lifted — no distinct JIT-specific symptom survives the fix, so
  `org/springframework/util/`'s blanket ban is removed rather than left
  liftable. Full writeup:
  `docs/internal/fixed-suite-bugs/spb1-springframework-util-investigation-FIXED.md`.

1. Verify (or refute) the TYPES-ERASURE.1 consolidation hypothesis — highest
   expected value, cheapest to test (no-rebuild env-var bisection already
   proven to work on this exact cluster).
2. Fresh `CRATONVM_JIT_DENY` bisection on ONE remaining SPB.x ban (SPB.1
   itself is DONE — removed 2026-07-26, see above — pick another, e.g.
   SPB.2 `org/springframework/core/`) to check whether the
   "allocate-then-putfield" theory still holds post the
   2026-07-25 TLAB-header finding, or whether it's stale like the JUnit
   iterator ban (and now SPB.1) turned out to be. Check the GC-root-scanning
   gap (`gc/src/vm_heap.rs::metadata_pin_deferrable`) first if the target
   package's classes are ever loaded via a user-defined `ClassLoader`.
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

   **BLOCKED 2026-07-26 ~02:35 UTC — no fixture on this host.** Exhaustively
   searched every jar on the Azure host (`find / -iname '*.jar' | xargs
   unzip -l | grep groovyjarjarantlr4/...`, zero matches) — the shaded
   `groovyjarjarantlr4` package this ban targets isn't present anywhere,
   including in `groovy-3.0.21.jar`/`groovy-3.0.8.jar`/`groovy-4.0.22.jar`
   (checked directly, no `antlr` entries at all in any of them — modern
   Groovy apparently ships this in a separate module not yet resolved on
   this host). Didn't want to speculatively fetch unknown additional
   dependencies to chase down which exact artifact has it. **Pivoting to
   TOMCAT-JNDIREALM-RDN.1/JIT.2 (`com/unboundid/`) instead** — real,
   working Tomcat Linux fixture already confirmed on this host (see
   `[[tomcat-linux-suite-fixture-location]]` memory /
   `docs/internal/jit-ban-sweep-20260725.md`), and the ban comment names
   an exact real repro (`TestJNDIRealmIntegration`, 76-case matrix).
   Leaving this ANTLR.1 item open/unclaimed for whoever has a Groovy
   fixture available, or is willing to fetch the right module.
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

**Result (2026-07-26 02:43 UTC): ban stays, re-confirmed real.**
Ran Tomcat's real 76-case `TestJNDIRealmIntegration` suite against this
host's fixture (`org.junit.runner.JUnitCore`, real UnboundID in-memory
LDAP server, no mocks): baseline (ban in place) 76/76 pass in ~35s. With
`CRATONVM_JIT_ALLOW_PACKAGES=com/unboundid/` (lifts both the narrow
RDN.getNameValuePairs guard and the broader JIT.2 whole-package guard at
once, since both gate on the same prefix): reproduces the documented
corruption exactly — `Stale pointer detected in invokevirtual receiver
(ptr=..., all-zero header) — falling back to CP class java/lang/String`,
followed by `ClassCastException(java.lang.Object cannot be cast to
java.lang.String)` during LDAP DN/RDN matching, and the run eventually
hangs (STW cross-thread JIT takeover waiting on cooperative mutators,
timeout at 120s). Confirmed JIT-specific: identical lifted config with
`--nojit` added is 76/76 clean in ~34s. "Do not attempt to lift without a
real fix for the stale-pointer/zero-header receiver bug."

**SUPERSEDED — BOTH BANS REMOVED 2026-07-26, root cause found and fixed
(`fix/tomcat-jndirealm-unboundid-jit-20260726`).** The real fix the entry
above asked for: **TOMCAT-JNDIREALM-JIT.3** — `JvmThread::string_case_cache`
(the ASCII case-conversion cache, three raw `ObjectRef`s per entry) was wired
into `roots.rs`/`gc.rs` only, i.e. the **initiator-only** pair, and into
neither published root snapshot nor the frozen-peer walk. Since the in-memory
LDAP server is thread-per-connection, the GC initiator is almost never the
cache's owner, so the non-moving young sweep reclaimed the cached Strings and
the owner's next `get_ascii_case_string_cached` returned the freed address.
Method bisection pinned the trigger to the single method
`com/unboundid/util/StaticUtils.toLowerCase` (whose whole body is the case
conversion): `CRATONVM_JIT_BISECT_SKIP` on it alone took the reclaimed-live
count from 3-4/run to 0 with everything else still compiled.

Two process lessons for the rest of this sweep:

1. **A passing run is not a clean run.** The 02:43 UTC "baseline 76/76 pass"
   and 38 later lifted runs that also passed were all emitting 3-4
   `Stale pointer` warnings each — the interpreter's CP-class fallback usually
   recovers. Grep stderr for `Stale pointer` and count it; that metric was
   near-deterministic where the assertion failure was ~40% flaky.
2. **Build a same-commit control before crediting a fix.** A control build at
   dev `e4e4053bb` still fails 4/5 in the identical harness, which is what
   makes "clean on current dev" mean something.

Full writeup:
`docs/internal/fixed-suite-bugs/tomcat/jndirealmintegration-unboundid-jit-corruption-FIXED.md`.

## JASPER-JDT.2 / JASPER-JDT.3 — CLAIMED 2026-07-26 02:54 UTC

Branch `fix/jit-ban-sweep-20260725`. Eclipse JDT parser (JASPER-JDT.2,
`org/eclipse/jdt/internal/compiler/parser/`) and AST/flow-analysis
(JASPER-JDT.3, `org/eclipse/jdt/internal/compiler/ast/`) package bans,
skip_list.rs ~L970-1022. Real Tomcat repros available on this host's
fixture: `org.apache.jasper.compiler.TestCompiler` (JASPER-JDT.2) and
`org.apache.catalina.authenticator.TestFormAuthenticatorA/B/C`
(JASPER-JDT.3).

**Result: TOMCAT-KEYEDLOCK-COMPUTE.1 landed 2026-07-26 03:35 UTC** (new
ban, not in the original ~46 -- see commit for details). Fixes
`TestCompiler` 8/12 -> 12/12.

**Residual, NOT yet fixed:** `TestFormAuthenticatorA`/`TestCompiler`
still hit a SECOND, independent JIT bug in
`org/apache/catalina/webresources/`: `AbstractResourceSet.checkPath`
throws `IllegalArgumentException: The requested path [/WEB-INF/...] is
not valid. It must begin with /` for a path that visibly DOES start
with `/` -- i.e. `path.charAt(0) != '/'` evaluates true when it
shouldn't. Confirmed JIT-only (checkPath itself, plus the whole
`webresources` package, when denied via `CRATONVM_JIT_DENY`, removes
this exact symptom). A synthetic `charAt(0) == '/'` stress probe
(500k iterations, various strings) did NOT reproduce it standalone, so
this needs the real call context (not yet bisected past the package
level) -- likely a similar family to the KEYEDLOCK-COMPUTE.1 fix just
landed (String read shortly after construction/mutation reading a stale
value) but not yet isolated to one method. Flagging for a future session
rather than continuing further given time already spent this session.

**Result: HIB-BIGINTEGER-AIOOBE.2 landed 2026-07-26 11:20 UTC** (widened
HIB-BIGINTEGER-AIOOBE.1's scope from MutableBigInteger-only to also cover
BigInteger itself -- see commit for the deterministic reproducer, the
first one this ban has ever had). This is a real correctness/performance
tradeoff for a widely-used JDK class; the ban can be narrowed back down if
someone root-causes the exact multi-method interaction (constructor +
some combination of trustedStripLeadingZeroInts/destructiveMulAdd/
checkRange/parseInt -- each ruled out alone, not yet narrowed further).

**Process note:** hit a real `cargo test` vs `cargo build --release`
staleness trap mid-investigation -- verifying a skip_list.rs change via
`cargo test` alone does NOT rebuild the separate `cratonvm` executable.
Always `cargo build --release` + check the binary's mtime before trusting
a still crashes/now passes result against a real repro.

## SPRING-HAZELCAST-XERCES-JIT.1 — CLAIMED 2026-07-26 11:26 UTC

Branch `fix/jit-ban-sweep-20260725`. `com/sun/org/apache/xerces/internal/`
(JDK-internal bundled Xerces) banned, skip_list.rs ~L1140-1152.
SchemaGrammar's SymbolHash corrupted under JIT during XSD schema
validation, NPE in getGlobalTypeDecl. Fully standalone (javax.xml.validation
+ a simple XSD, no external deps) -- building a stress repro.

**Result: SPRING-HAZELCAST-XERCES-JIT.1 REMOVED, stale 2026-07-26 11:59
UTC.** Re-verified with a standalone javax.xml.validation stress probe
(4 and 32 distinct XSD schemas, ~40k total Validator.validate() calls);
confirmed SymbolHash.hash/.search/.get -- the exact class the original
bug named -- actively JIT-compiled throughout via CRATONVM_DBG_JITC=1,
no crash. Merged dev@9ba6ab5de.


## Session update 2026-07-26 (continuation, `fix/jit-ban-remaining-20260726`)

Worked the remaining unclaimed candidates from the "Not investigated this
session" list above. Full writeup: `docs/internal/jit-ban-remaining-sweep-20260726.md`.

- **REMOVED (7, real-jar/real-checkout-verified, baseline/lifted/aggressive-threshold all clean):**
  SPR-AOT-TESTNG-MAPS.1, REACTOR-ADDCAP.1, REACTOR-FLUXCREATE.1, JETTY-WSIO.1,
  SPRINGBOOT-WITHOUT-JACKSON.2 (redundant/shadowed by the separate
  `org/springframework/boot/` blanket ban under Conservative — safe no-op for
  default behavior, see the doc for the nuance), ES-HAMCREST.1, SnakeYAML
  emitter (ES-JIT-DEOPT-GC.1).
- **KEPT, confirmed still live:** JAXB (`org/glassfish/jaxb/`) — real
  corruption reproduced once a newly-found, unrelated `java.io.Writer`
  bug (below) was worked around.
- **KEPT, no fixture to test:** ES fragile cluster / whole `org/elasticsearch/`
  prefix — no Elasticsearch checkout survives on this host; see
  `docs/known-issues/es-fragile-cluster-no-fixture-20260726.md`.
- **Already moot, no action:** BC-ASN1.1 and FELIX.1 both live inside
  `is_known_miscompile`, gated behind `callee_saved_gpr_local_homes_enabled()`
  (defaults `false`) — dead in any default run already. NETTY.1's only
  matching entry was the historical `Arrays.fill` bug, already lifted
  2026-06-11 (predates this session).
- **NEW BUG FOUND:** `java.io.Writer.write(char[])` silently drops output
  under JIT once hot — a general VM defect, not app-specific, unrelated to
  the JAXB ban it was found under. Not yet root-caused/fixed. See
  `docs/known-issues/java-io-writer-write-char-array-jit-miscompile-20260726.md`.

Reproducers committed under `docs/known-issues/repros/jitban-remaining-20260726/`.
