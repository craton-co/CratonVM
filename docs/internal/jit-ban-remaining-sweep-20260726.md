# JIT ban sweep — remaining unclaimed candidates (2026-07-26, continuation)

Continuation of the "remove all app-specific JIT bans, fix newly discovered
bugs" effort (`docs/internal/jit-ban-sweep-20260725.md`,
`docs/known-issues/jit-skip-list-open-bans-20260725.md`) after both prior
sessions' branches (`fix/jit-ban-sweep-20260725`, `fix/jit-ban-sweep2-20260726`)
were merged to `origin/dev`. Worked the user's explicit remaining-unclaimed
list: JAXB, SnakeYAML emitter, ES-HAMCREST.1, ES fragile cluster,
SPR-AOT-TESTNG-MAPS.1, REACTOR-ADDCAP.1/FLUXCREATE.1, JETTY-WSIO.1, NETTY.1,
FELIX.1, BC-ASN1.1, SPB-FLYWAY-HSQLDB.1, SPRINGBOOT-WITHOUT-JACKSON.2.

Worktree: `/data/data/wt-jitban-remaining-20260726`, branch
`fix/jit-ban-remaining-20260726`, from `origin/dev`. Binaries:
`/data/tmp/jitban2-bins/cratonvm-jitban2-{baseline,diag1}-20260726`. Probes:
`/data/tmp/jitban2-probes/` on the Azure host (real third-party jars,
compiled repros, no Elasticsearch checkout available — see below).

## Pre-check: three candidates were already inert / already resolved

- **BC-ASN1.1** (`java/util/Calendar.isFieldSet`) and **FELIX.1**
  (`java/lang/reflect/AccessibleObject.setAccessible` /
  `SecureAction.lambda$getAccessor$0`) both live inside `is_known_miscompile`,
  which is only even called when `callee_saved_gpr_local_homes_enabled()`
  returns true — and that defaults to `false` on x86_64
  (`CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS` unset). Neither ban affects
  any default JIT run today; nothing to lift. Left as-is (dead diagnostic
  entries, matching this file's existing convention for the rest of that
  gated block).
- **NETTY.1** (`java/util/Arrays.fill` counted-loop miscompile) was already
  fully lifted on 2026-06-11 (see the "NETTY.1 LIFTED" comment already in the
  file, predating this session) — the entry in the user's candidate list is
  a stale historical name with no corresponding active check left. No
  `io/netty/` package ban of any kind exists in current `skip_list.rs`.

## Diagnostic prep: added `package_allowed()` escape hatches

Five targeted bans (in the "both policies" block, before the
`SkipPolicy::Conservative` gate) had no `CRATONVM_JIT_ALLOW_PACKAGES` lift
mechanism at all — unlike almost everything else in the file, testing them
required a source edit + rebuild per hypothesis. Added a
`&& !package_allowed(...)` guard to each (one source edit, one rebuild,
reused for all five afterward, matching the file's dominant convention):
`SPRINGBOOT-WITHOUT-JACKSON.2`, `SPR-AOT-TESTNG-MAPS.1`, `REACTOR-ADDCAP.1`,
`REACTOR-FLUXCREATE.1` (both entries), `JETTY-WSIO.1`. This is a pure
diagnostic addition (default behavior unchanged; `CRATONVM_JIT_ALLOW_PACKAGES`
defaults empty) — all five were then fully re-verified real-app/real-jar
positive and their bans removed outright (see below), so the escape hatches
themselves are gone again along with the removed checks.

## REMOVED (7) — confirmed safe with real third-party jars, no fixture needed

All tested baseline (ban active) vs. lifted (`CRATONVM_JIT_ALLOW_PACKAGES`)
vs. a `CRATONVM_JIT_THRESHOLD=1` aggressive-compilation pass. 0 failures in
every configuration for all seven:

| Ban | Real dependency used | Probe |
|---|---|---|
| SPR-AOT-TESTNG-MAPS.1 | testng-7.12.0.jar | `TestNgMapsProbe.java` |
| REACTOR-ADDCAP.1 / REACTOR-FLUXCREATE.1 | reactor-core-3.8.6.jar | `ReactorAddCapProbe.java` |
| JETTY-WSIO.1 | jetty-12.1.10 (server + client + websocket, embedded) | `JettyWsIoProbe.java` |
| ES-HAMCREST.1 | hamcrest-core/hamcrest/hamcrest-library 3.0 | `HamcrestProbe.java` |
| ES-JIT-DEOPT-GC.1 (SnakeYAML emitter) | snakeyaml-1.33.jar + jackson-dataformat-yaml-2.18.8.jar | `SnakeYamlEmitProbe.java` |
| SPB-FLYWAY-HSQLDB.1 | hsqldb-2.7.4.jar | `FlywayHsqldbProbe.java` (direct JDBC — Flyway 12.4.0 dropped built-in HSQLDB support, no plugin jar on this host; see doc below) |

**SPRINGBOOT-WITHOUT-JACKSON.2 is a special case, not like the other six.**
Testing with the real `spring-boot-test-support` 7.0.7 module
(`SpringBootLoadClassProbe.java`) showed 0 failures baseline vs. lifted, but
that differential turned out to be uninformative: `ModifiedClassPathClassLoader`
is *also* caught by a separate, much broader, pre-existing
`org/springframework/boot/` blanket ban (excluding `.../boot/loader/`,
`vm/src/jit/skip_list.rs` ~line 1601, not part of this session's candidate
list) that lives inside the `SkipPolicy::Conservative`-only block. Under the
default (and only CLI-reachable) Conservative policy, this class stays
interpreted regardless of whether the narrow SPRINGBOOT-WITHOUT-JACKSON.2
guard exists — so the probe never actually got to exercise the method under
JIT. The removal is still safe (Conservative/default behavior is unchanged;
the narrow guard was fully redundant/shadowed), and it does have one real
effect — under `SkipPolicy::Aggressive` (only reachable from Rust unit
tests today, `jit_aggressive_compilation` has no CLI/env wiring) the method
is now JIT-eligible where it previously wasn't — but this is a
"confirmed-redundant, safe no-op for default behavior" finding, not an
independently-JIT-verified one like the other six. See the corrected unit
test (`spring_boot_modified_classpath_loader_is_jit_eligible_after_removal`)
for the exact before/after per policy.

Code changes: `vm/src/jit/skip_list.rs` — each `if` block replaced with a
`-- REMOVED 2026-07-26` comment (following this file's established
convention for prior removals like `PROXY-JITCALL.1`), corresponding unit
tests rewritten from "stays interpreted" assertions to
"is_jit_eligible_after_removal" regression witnesses.

## KEPT (2) — confirmed still needed

- **JAXB** (`org/glassfish/jaxb/`, `jaxb_mapping_residual_skip_prefix`) —
  confirmed still live: with the ban lifted AND a newly-discovered unrelated
  `java.io.Writer` bug (below) worked around, a real
  `jakarta.xml.bind`/`org.glassfish.jaxb` 4.0.7 marshal/unmarshal loop over a
  `QName`-bearing class hits a distinct JIT-only `UnmarshalException:
  unexpected element` (QName compares unequal despite identical text) at
  iteration 81 of 4000. With the ban active (default) and the same
  `Writer`-bug workaround, the identical probe is clean 4000/4000. `--nojit`
  with the ban lifted is clean 1000/1000, confirming JIT-specificity.
- **ES fragile cluster** (`is_elasticsearch_suite_jit_fragile_cluster`, whole
  `org/elasticsearch/` prefix) — **not re-tested**, no Elasticsearch checkout
  exists on this host (prior sessions' worktree references are gone,
  presumably pruned). See
  `docs/known-issues/es-fragile-cluster-no-fixture-20260726.md`. Left
  banned; the narrower `ES-HAMCREST.1` in the same block WAS independently
  re-verified and removed (see above), since Hamcrest's own matcher codegen
  is testable without an ES checkout.

## NEW BUG FOUND: `java.io.Writer.write(char[])` JIT miscompile

Found incidentally while re-testing the JAXB ban. Full writeup:
`docs/internal/java-io-writer-write-char-array-jit-miscompile-20260726.md`
(CLOSED 2026-07-27 — does not reproduce; see also
`docs/internal/jit-licm-preheader-bypass-20260727.md`, the general JIT bug
that the same probe was really failing on at 4000 iterations).
Summary: `Writer.write(char[] cbuf)`'s trivial one-line forwarding body
(`write(cbuf, 0, cbuf.length)`, inherited by `StringWriter` since it doesn't
override this specific zero-argument-array overload) silently drops the
entire write once its call site is hot enough to JIT — reproduces
deterministically at iteration 26 of a JAXB marshal loop, isolated via
`CRATONVM_JIT_BISECT_ONLY=java/io/Writer` (compiling ONLY this one class,
everything else interpreted, still reproduces) down to the `write` method
family specifically (`CRATONVM_JIT_BISECT_SKIP=java/io/Writer.write` clears
it; `.append`/`.close`/`.flush` do not). Not yet root-caused at the codegen
level — no fix attempted, flagged for a dedicated follow-up session. This is
a **general VM bug**, not app-specific, and is unrelated to (does not
subsume or get subsumed by) the JAXB ban.

## Reproducers

All committed under `docs/known-issues/repros/jitban-remaining-20260726/`:
`TestNgMapsProbe.java`, `ReactorAddCapProbe.java`, `JettyWsIoProbe.java`,
`SpringBootLoadClassProbe.java` (package
`org.springframework.boot.testsupport.classpath`), `HamcrestProbe.java`,
`SnakeYamlEmitProbe.java`, `FlywayHsqldbProbe.java` (+ its
`db/migration/*.sql` — unused by the final direct-JDBC version, kept for
reference), `JaxbQNameProbe.java`.
