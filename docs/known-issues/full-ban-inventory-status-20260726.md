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

## CONFIRMED still needed, KEPT, each with a dedicated writeup

- JAXB (`org/glassfish/jaxb/`) — `docs/known-issues/jaxb-still-needed-20260726.md`
- ES fragile cluster (`org/elasticsearch/`) — no fixture, `docs/known-issues/es-fragile-cluster-no-fixture-20260726.md`
- JSONSMART-PARSER.1 (`net/minidev/json/parser/`) — `docs/known-issues/jsonsmart-parser-still-needed.md`
- SPB.1 (`org/springframework/util/`) — inconclusive real-app-less repro, `docs/known-issues/spb1-springframework-util-investigation.md`
- TOMCAT-JNDIREALM-RDN.1 / JIT.2 (`com/unboundid/`) — real Tomcat suite, SIGSEGV confirmed, see `docs/known-issues/jit-skip-list-open-bans-20260725.md`
- `org/jboss/as/` (WildFly boot, part of the SPB.8b/8c family) — real WildFly boot, `ModelTypeValidator.validTypes` NPE, `docs/known-issues/wildfly/modeltypevalidator-validtypes-npe.md`; WILDFLY-CONTROLLER-JIT.1 transitively confirmed via the same finding
- `org/h2/` + `org/antlr/v4/runtime/` (HIB-LONGTAIL.1) — real 218-class H2 suite, `Schema not found` reconnect bug, `docs/known-issues/h2/h2-jitban-schema-not-found-on-reconnect.md`
- HIB-BIGINTEGER-AIOOBE.1/.2 (`java/math/{BigInteger,MutableBigInteger}`) — deterministic repro, no escape hatch by design; now cross-referenced with SUNEC-INTPOLY above
- TYPES-ERASURE.1 (`com/sun/tools/javac/code/Types.erasure`) — 40/40 repro; consolidation-with-the-other-6-javac-bans hypothesis explicitly REFUTED (see `docs/known-issues/jit-skip-list-open-bans-20260725.md`), so it stays as its own entry alongside SPRING-TESTCOMPILER.1-4/HIB-STOREDPROC-JIT.1 below

## CONSOLIDATED FINDING — no longer individually actionable

`is_known_miscompile()`'s entire ~950-line `matches!` block is unreachable
dead code under any default run (gated behind
`callee_saved_gpr_local_homes_enabled()`, defaults false, no CLI wiring).
This resolves EXEC.1, W2-CHM, RBC.1, HIB-PROXY, KC26.LR, KC-CRED.LAZY,
ES-HANG-01's WeakHashMap entries, and the SPB.1/2/8 *individual-method*
entries (HashMap/LinkedHashMap/String/Provider/Long/Integer) without
further work needed. Full writeup:
`docs/known-issues/is-known-miscompile-block-inert-20260726.md`.
BC-ASN1.1, FELIX.1, and JUNIT.1's duplicate dead copy were the specific
instances found and confirmed as part of this same discovery.

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
