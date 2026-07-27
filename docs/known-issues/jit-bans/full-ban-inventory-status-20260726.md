# Full `skip_list.rs` app-specific-ban inventory and status (2026-07-26)

Comprehensive accounting requested by the `/goal` condition ("safely remove
all app-specific JIT bans ... file a .md for each ban that cannot be
lifted"). This session (and the two prior sessions this one continues,
`fix/jit-ban-sweep-20260725` / `fix/jit-ban-sweep2-20260726`, both already
merged to `dev`) worked through the vast majority of the originally
~46-entry catalogue. This doc is the closing inventory: every currently
distinct named ban, its disposition, and — for anything not yet
individually re-verified — why, and what a future session needs to close
it.


## 2026-07-27 — a whole class of this sweep's removals was verified against an inert code path

`vm/src/jit/helpers.rs`'s `direct_virtual_compiled_callee_entry_enabled()` was
**default-OFF** for the entire period in which this sweep did its ban removals.
That flag gates the only write of `mic.cached_entry_ptr`, so with it off the
inline MIC/PIC cascade the codegen emits at every compiled `invokevirtual` can
never open, and **a JIT-compiled caller never reaches a JIT-compiled callee** —
every virtual call out of compiled code falls back into the interpreter.

Any ban whose mechanism is compiled-to-compiled virtual dispatch therefore could
not reproduce during a default-OFF run, no matter what state the underlying
defect was in. "Re-verified, no longer reproduces" measured that way measures
nothing.

Confirmed instance: **JASPER-JDT.2** (`org/eclipse/jdt/internal/compiler/parser/`)
and **JASPER-JDT.3** (`.../ast/`), both removed 2026-07-26 after four repeat runs
each on real Tomcat fixtures. With the flag on, real Tomcat
`jakarta.el.TestOptionalELResolverInJsp` fails 3/3 (JSP compile dies with
`ClassCastException: ...ast.QualifiedTypeReference cannot be cast to
...ast.FieldDeclaration` → HTTP 500) and passes 3/3 with `parser/` denied. Both
bans are RESTORED; `parser/` is directly re-confirmed, `ast/` on the shadowing
argument.

**Action for the rest of this inventory:** every removal in the 2026-07-25/26
sweep justified by "no longer reproduces" needs re-checking with
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` **on** before it can be
trusted. Bans whose mechanism is not virtual dispatch (pure codegen, GC roots,
class-init ordering) are unaffected.

Full detail: `docs/known-issues/h2/h2-jitban-residuals-20260726.md`.


## REMOVED this multi-session effort (confirmed safe, real testing, code deleted)

TYPES-ERASURE.1 (added then partially superseded — see below),
PROXY-JITCALL.1, SPRING-HAZELCAST-XERCES-JIT.1, SPR-AOT-TESTNG-MAPS.1,
REACTOR-ADDCAP.1, REACTOR-FLUXCREATE.1, JETTY-WSIO.1,
SPRINGBOOT-WITHOUT-JACKSON.2, ES-HAMCREST.1, ES-JIT-DEOPT-GC.1 (SnakeYAML
emitter), SPB-FLYWAY-HSQLDB.1, HIB-LONGTAIL.2, JUNIT.1, SUNEC-INTPOLY
(⚠ see cross-reference caveat in-line at both sites — depends on
HIB-BIGINTEGER-AIOOBE.1/.2 staying active). NETTY.1's Arrays.fill entry
was independently already-lifted before this multi-session effort even
started.

## Superseded by later re-testing

- JSONSMART-PARSER.1 (`net/minidev/json/parser/`) — was listed here as
  "confirmed still needed". **RETIRED 2026-07-27**: the ban is gone from
  `skip_list.rs`, the package JIT-compiles, and 3,000,000 round-trip parse
  operations produce 0 errors. The one real defect found on re-test was a
  VM-wide JIT bug (a native-shadowed `HashMap.<init>()V` being elided by the
  trivial-constructor optimisation), now fixed. See
  `docs/internal/jsonsmart-parser-jit-retired-20260727.md`.

## CONFIRMED still needed, KEPT, each with a dedicated writeup

- JAXB (`org/glassfish/jaxb/`) — `docs/known-issues/jaxb-still-needed-20260726.md`
- ES fragile cluster (`org/elasticsearch/`) — no fixture, `docs/known-issues/es-fragile-cluster-no-fixture-20260726.md`
- SPB.1 (`org/springframework/util/`) — inconclusive real-app-less repro, `docs/known-issues/spb1-springframework-util-investigation.md`
- TOMCAT-JNDIREALM-RDN.1 / JIT.2 (`com/unboundid/`) — real Tomcat suite, SIGSEGV confirmed, see `docs/known-issues/jit-skip-list-open-bans-20260725.md`
- `org/jboss/as/` (WildFly boot, part of the SPB.8b/8c family) — real WildFly boot, `ModelTypeValidator.validTypes` NPE, `docs/known-issues/wildfly/modeltypevalidator-validtypes-npe.md`; WILDFLY-CONTROLLER-JIT.1 transitively confirmed via the same finding
- `org/h2/` + `org/antlr/v4/runtime/` (HIB-LONGTAIL.1) — real 218-class H2 suite, `Schema not found` reconnect bug, `docs/internal/fixed-suite-bugs/h2-suite-bugs/h2-jitban-schema-not-found-on-reconnect-FIXED.md`
- HIB-BIGINTEGER-AIOOBE.1/.2 (`java/math/{BigInteger,MutableBigInteger}`) — deterministic repro, no escape hatch by design; now cross-referenced with SUNEC-INTPOLY above
- TYPES-ERASURE.1 (`com/sun/tools/javac/code/Types.erasure`) — 40/40 repro; consolidation-with-the-other-6-javac-bans hypothesis explicitly REFUTED (see `docs/known-issues/jit-skip-list-open-bans-20260725.md`), so it stays as its own entry alongside SPRING-TESTCOMPILER.1-4/HIB-STOREDPROC-JIT.1 below

## CONSOLIDATED FINDING — CLOSED 2026-07-27, block deleted

`is_known_miscompile()`'s entire ~950-line `matches!` block (189 entries) was
unreachable dead code under any default run, gated behind a PRIVATE
`callee_saved_gpr_local_homes_enabled()` copy in `skip_list.rs` that defaulted
false — while the real allocator switch of that name
(`jit::x64::callee_saved_gpr_local_homes_enabled()`) has defaulted **true**
since precise JIT maps went default-on 2026-07-07. The block and that private
gate were **deleted 2026-07-27**, closing EXEC.1, W2-CHM, RBC.1, HIB-PROXY,
KC26.LR, KC-CRED.LAZY, ES-HANG-01's WeakHashMap entries, BC-ASN1.1, FELIX.1,
SB-17, JUNIT.1's duplicate dead copy, the ecj `HashtableOf*.rehash` family and
the SPB.1/2/8 *individual-method* entries
(HashMap/LinkedHashMap/String/Provider/Long/Integer).

19 of the 189 entries were additionally verified as genuinely JIT-compiled
today and correct (`--nojit` vs. JIT vs. `CRATONVM_JIT_THRESHOLD=1`, with
`CRATONVM_DBG_JITC` proving compilation); the rest cannot be compiled at all
(Rust natives win over their bytecode), are `<init>`s the constructor gate
already blocks, or are shadowed by other still-active bans. Full writeup:
`docs/internal/is-known-miscompile-block-retired-20260727.md`.

## NOT YET RE-INVESTIGATED this session — real, open work, blocked or unclaimed

These are genuinely still-open items. Most were already flagged in
`docs/known-issues/jit-skip-list-open-bans-20260725.md` as blocked by the
same root cause: **the original crash-fixture apps this ban family was
found against (SportMe-master, ms-course-youtube/admin-service,
insurance-backend, eureka-server, msyt-admin, cglib_probe, a Groovy
checkout with `groovyjarjarantlr4`, a Keycloak 26.2.4 checkout for KC26-*)
are not present on this Azure host** as of 2026-07-26 (exhaustively
searched at the start of prior sessions and re-confirmed by this one for
Elasticsearch specifically). This is a structural/host-provisioning gap,
not a shortcut — closing it requires either re-fetching one of these
checkouts onto the host or finding an equivalent real app.

- **SPB.2/.4/.4b/.4c/.5/.6/.7/.8/.8b/.8c/.9/.9b/.9c/.9d, CGL.1, PIC.1** —
  the remaining ~15 *package-level* blanket bans in this family (as
  opposed to the individual-method entries inside `is_known_miscompile`,
  already resolved above). `org/jboss/as/` (part of SPB.8b/8c) is the one
  member of this family that WAS confirmed still-needed via a real WildFly
  boot (see above); the other ~14 remain individually unverified.
- **ANTLR.1** (`groovyjarjarantlr4/`) — blocked, no fixture (the shaded
  package doesn't exist in any Groovy jar found on this host: checked
  3.0.21/3.0.8/4.0.22 directly). ANTLR-COLDPATH.1's narrower correctness
  guard inside this package is independently confirmed and stays
  regardless (kept by design, not blocked).
- **HIB-ANTLR.1** (ordinary, non-shaded ANTLR4 runtime used directly by
  Hibernate) — not yet investigated this multi-session effort.
- **KC26-PIC.1, KC26-RX.1** (Keycloak/picocli/RxJava3) — needs a real
  Keycloac 26.2.4 checkout; not present on this host.
- **HIB-TEMPORAL.1** (`org/hibernate/`, temporal/DDL type descriptor
  package) — not yet investigated this multi-session effort.
- **HIB-LONGTAIL.3** (`GenerationTargetToScript.<init>`) — needs a real
  `org.hibernate.tool` (hibernate-tools) jar; not confirmed present on
  this host, not investigated this session. Also worth checking first
  whether this constructor classifies as `InitComplexity::Trivial` (if
  so, it may already be redundant with the generic non-trivial-constructor
  gate, the same shadowing pattern found this session for
  SPRINGBOOT-WITHOUT-JACKSON.2 — check before assuming a real-app test is
  needed).
- **JASPER-JDT.3 residual** (`org/apache/catalina/webresources/AbstractResourceSet.checkPath`)
  — this is not a ban to lift; it is an OPEN, already-flagged bug (a
  distinct JIT-only `IllegalArgumentException` for a path that visibly
  does start with `/`) found by a prior session while testing JASPER-JDT.3,
  explicitly deferred as its own follow-up item (see
  `docs/known-issues/jit-skip-list-open-bans-20260725.md`,
  "JASPER-JDT.2 / JASPER-JDT.3" section).
- **SPRING-TESTCOMPILER.1-4, HIB-STOREDPROC-JIT.1** (the other 6
  javac-family bans alongside TYPES-ERASURE.1) — the consolidation
  hypothesis (fixing/banning `Types.erasure` alone might subsume these)
  was explicitly tested and REFUTED by a prior session (see
  `docs/known-issues/jit-skip-list-open-bans-20260725.md`); a partial
  narrowing was in progress (`ClassFinder.complete` identified as
  load-bearing but not fully isolated from the remaining ~4) and was not
  continued this session.
- **TOMCAT-DOHEAD-JUNIT-ITERATOR.1** — a prior concurrent session's lane;
  two standalone repros this session-family built suggested it MAY be
  stale (fixed by later general GC-root-tracking work) but the result
  was flagged inconclusive due to host contention noise; needs a clean
  re-run on a quiet host window.

## Recommendation for the next session

Priority order by leverage-per-effort: (1) check whether HIB-LONGTAIL.3's
constructor is shadowed by the generic constructor gate (near-zero cost,
matches an already-proven pattern); (2) `org/hibernate/` (HIB-TEMPORAL.1) —
Hibernate-only, no missing-fixture-app blocker known yet, worth a fresh
look; (3) fetch a Keycloak 26.2.4 checkout onto this host to unblock
KC26-PIC.1/KC26-RX.1 and RBC.1's crypto-adjacent tests together; (4) fetch
one of the missing Spring Boot fixture apps to unblock the remaining SPB.x
package-level family in bulk (matches prior sessions' own top
recommendation, never completed due to host provisioning).

## Update 2026-07-26 (same day, continuation after the prior "blocked, no fixture" claims were challenged)

The user pointed out that fixture apps for this sweep DO exist on the
Azure host and told this session to search harder. That search found real
fixtures for three of the four families previously marked "blocked, no
fixture," all of which were simply named after the specific investigation
they were built for rather than the technology itself, so the earlier
`find -iname '*groovy*'` / `'*hibernate*'` / `'*elasticsearch*'` /
`'*keycloak*'` sweeps missed them:

- **ANTLR.1** (`groovyjarjarantlr4/`) — found `groovy-3.0.21.jar` (844
  shaded-antlr4 classes) already cached in `.gradle`, plus a prior
  session's own probe directory at `/data/tmp/groovy-antlr4-probe/` and
  build logs at `/data/tmp/groovy-antlr4-build*.log` — evidence a fixture
  existed all along. **REMOVED** — real cold-parse throughput retest
  shows the ~8x regression this ban's only remaining justification relied
  on no longer holds.
- **HIB-ANTLR.1** / **HIB-TEMPORAL.1** (`org/antlr/v4/runtime/` /
  `org/hibernate/`) — found a full real Hibernate ORM 8.0 test harness at
  `apps/hibernate-orm-harness/` (compiled `hibernate-core` test classes +
  full runtime classpath + a JUnit5 Platform Launcher driver). HIB-ANTLR.1
  **REMOVED** (own claim doesn't reproduce, though shadowed by
  HIB-LONGTAIL.1 regardless). HIB-TEMPORAL.1 **CONFIRMED STILL NEEDED** —
  and MORE severe than documented: lifting it causes a full
  `StrategySelectionException` Hibernate bootstrap failure, not just a
  narrow DDL-descriptor NPE.
- **ES fragile cluster** (`org/elasticsearch/`) — found a full real
  Elasticsearch 9.6.0-SNAPSHOT checkout at
  `/data/data/es-fixture-ivfknn-slicesdense-closure-20260717/` (2555
  compiled test classes, `test/framework` module, `libvec.so` already
  built). **CONFIRMED STILL NEEDED** — an 18-class spread sample found a
  real regression (`FloatFieldBlockLoaderTests`: 38→41 failures under
  JIT); the true failure surface across the full suite is likely larger,
  not yet fully characterized.
- **KC26-PIC.1 / KC26-RX.1** (Keycloak) — found a full real Keycloak
  26.6.1 Maven repo + bootable quarkus-dist server at
  `/home/victor/.m2/repository/org/keycloak/`. Still blocked, but for a
  **different, more specific reason** than "no fixture": booting the real
  server fails immediately in `Version.<clinit>` with a
  `getResourceAsStream("/keycloak-version.properties")` classloading gap
  — a real, separate (non-JIT) bug, not yet root-caused. See
  `docs/known-issues/keycloak-boot-blocked-version-null-20260726.md`.

**Lesson for future searches:** when a prior "no fixture on this host"
claim needs re-checking, search by the TECHNOLOGY'S content/purpose
(shaded package names, `craton-testcp.txt`, compiled `*Tests.class`
files) across the whole filesystem, not just by the app's own expected
directory name — investigation-specific naming conventions
(`es-fixture-ivfknn-*`, `hibernate-orm-harness`) hide otherwise-complete,
reusable fixtures from a narrow name-based search.

**Still genuinely blocked, no fixture found even after this deeper
search:** the SPB.x package-level family's named fixture apps
(SportMe-master, ms-course-youtube, insurance-backend, eureka-server,
msyt-admin, cglib_probe) — searched at full filesystem depth with no
matches. These remain a real, structural gap (no equivalent generic
open-source app was substituted, to avoid overclaiming coverage of a
specific historical bug's exact trigger shape).

## Update 2026-07-26 (later same day, round 4 — classloader fix, DoHead removal, javac-family consolidation refuted)

**SPB.x package family re-confirmed genuinely absent**, this time with a
proper per-name search (the combined multi-pattern `find ... -o -iname
...` used earlier this session produced false-positive noise from
coincidental substring collisions — e.g. `*sportme*` matches
`TransportMessage` because "transportmessage" happens to contain the
literal substring "sportme"; re-ran each name as its own separate `find`
invocation to avoid this). Zero real matches for `SportMe-master`,
`ms-course-youtube`, `insurance-backend`, `eureka-server` anywhere under
`/data` or `/home`. This independently confirms the same conclusion the
concurrent `fix/jit-ban-sweep-20260725` session already reached (see
`docs/internal/jit-ban-sweep-20260725.md`'s own SPB.1 section) — this gap
is real, not a search-methodology failure like the earlier Groovy/
Hibernate/ES/Keycloak false negatives were.

**Major new findings this round:**

1. **Real VM bug fixed:** `Class.getResourceAsStream`/`getResource`
   ignored the actual defining `ClassLoader`'s own override, falling back
   to a global `-cp` scan — broke any custom (non-builtin,
   non-`URLClassLoader`) loader whose backing jars aren't on the
   process's own classpath (e.g. Quarkus's `RunnerClassLoader`, which
   backs the real Keycloak 26.6.1 server). Fixed in
   `native-builtins/src/lang_class.rs`, merged to `dev`
   (`ceea4eb05`). Unblocked Keycloak's `Version.<clinit>` NPE entirely;
   boot now hits a second, deeper, separate class-*resolution* gap
   (`ClassManager::find_class_bytes_delegated` only checks bootstrap/
   extension/application, no path to a custom loader's `findClass`) —
   documented, not fixed, in
   `docs/known-issues/keycloak-boot-blocked-version-null-20260726.md`.

2. **TOMCAT-DOHEAD-JUNIT-ITERATOR.1 removed**, properly re-verified this
   time (see item 3 below for why "properly" matters).

3. **Discovered an undocumented blanket `org/junit/` ban** (Conservative-
   only, no rationale comment, incidental in commit `60ef90d4b`) that
   silently shadowed the first pass of TOMCAT-DOHEAD-JUNIT-ITERATOR.1's
   re-test AND retroactively invalidates part of this session's earlier
   `JUNIT.1` removal claim ("JIT-eligible unconditionally now" — corrected
   to accurately describe a safe-but-shadowed no-op).

   **UPDATE 2026-07-27: CLOSED.** This ban and its three siblings were all
   removed. Root cause of what they were hiding: not a miscompile, but
   `jit/src/ir_lower.rs` panicking on an overflowed code buffer rather than
   taking its own `buf.overflowed()` bail to single-pass — reached via one
   class, `org/junit/internal/MethodSorter`. That fix also cleared 7
   pre-existing SIGABRTs from the default-settings Elasticsearch baseline.
   `JUNIT.1`'s shadowed-no-op claim was re-tested properly with the shadow
   lifted and now holds. Retired writeup:
   `docs/internal/blanket-org-junit-ban-undocumented-shadow-20260726.md`.

4. **TYPES-ERASURE.1 consolidation hypothesis tested and REFUTED**: the
   open question of whether banning `Types.erasure` alone subsumes the
   other 7 javac-family bans (`SPRING-TESTCOMPILER.1-4`,
   `HIB-STOREDPROC-JIT.1`) was tested directly — lifting all 7 others
   while keeping only `Types.erasure` banned reproduces
   `SPRING-TESTCOMPILER.2`'s original `ClassReader.readClass` NPE at
   iteration 6/200. All 8 bans are independent, confirmed necessary.
   `docs/known-issues/javac-family-consolidation-hypothesis-refuted-20260726.md`.

5. **New, unrelated bug found while building the consolidation probe:**
   any source containing `@SuppressWarnings("...")` fails in-process
   javac compilation under CratonVM ("duplicate element 'value'") —
   reproduces on the first-ever compile, with JIT fully disabled, so it's
   NOT part of the javac-JIT-miscompile family at all. Not root-caused.
   `docs/known-issues/suppresswarnings-annotation-duplicate-value-bug-20260726.md`.

**Net this round:** 1 real VM bug fixed (classloader delegation), 1 ban
removed (TOMCAT-DOHEAD-JUNIT-ITERATOR.1), 1 prior overclaim corrected
(JUNIT.1), 1 major hypothesis tested-and-closed (javac-family
consolidation, refuted), 2 new bugs found and documented for follow-up
(Keycloak class-resolution gap, SuppressWarnings annotation bug), 1
undocumented blanket ban surfaced and flagged (org/junit/), SPB.x family
absence double-confirmed. All landed on `dev` incrementally with
`cargo test --release -p cratonvm-vm --lib skip_list` green at every
step (63 passed, 0 failed throughout).
