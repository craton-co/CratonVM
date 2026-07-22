# `hql.ASTParserLoadingTest` HANG — genuine (not stuck) severe slowdown in JAXB/HBM-XML model-building reflection storm; same `update_root_snapshot` mechanism as the Tomcat/Spring-Boot throughput wall, worse than any previously-recorded instance

| | |
|---|---|
| **Status** | 🔴 **OPEN** — not a new/distinct bug; a new, notably severe trigger of the already-documented, already-OPEN `update_root_snapshot` reflection-storm throughput wall ([`docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`](../../internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md)). |
| **Class** | `org.hibernate.orm.test.hql.ASTParserLoadingTest` |
| **Symptom this run** | `HANG`, `process-died rc=124` — harness's flat 300s per-invocation timeout expired with **zero** `@@RESULT`/`@@FAIL` ever printed. |
| **Verdict** | Genuinely, continuously CPU-bound the entire time — **not** stuck/deadlocked. Root cause: deeply-nested (130+ interpreter frames) JAXB reflection-based model building for this class's large `@DomainModel` (16 legacy `.hbm.xml`/`.orm.xml` mappings + a 6-level joined-subclass `Animal` hierarchy + 3 annotated classes), which never finished its `SessionFactory` bootstrap phase within the observed window — let alone reached any of its 106 `@Test` methods. |

## Source run

`apps/hib-suite-runner/runs/run-20260721-175909-passed/on-real/shard-4/raw.log`
(idx 92, `@@BEGIN 92 org.hibernate.orm.test.hql.ASTParserLoadingTest` at line 44161,
next class's process starts at line 48363 — this batch invocation was killed
by the wrapper's `timeout 300` while `ASTParserLoadingTest` was in progress).
Binary from worktree `CratonVM-hib-local-0712` (branch `test/hib-local-0712`,
merged with `origin/dev` @ `7aed580f0`).

The raw shard log's last content before the kill is a run of
`WARN [org.hibernate.orm.deprecation] HHH90000028: Support for <hibernate-mappings/>...`
lines for this class's 9 `.hbm.xml` files (`Animal.hbm.xml` through
`legacy/Marelo.hbm.xml`) — i.e. it hadn't even finished the JAXB parse of its
own mapping files before the shared batch's 300s wall-clock budget (shared
across however many earlier classes that same invocation had already
processed) ran out.

## Solo reproduction

Ran standalone with the exact same binary/flags, single-class list, generous
budget (`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 900 ... --stack-dump-on-timeout 300 CratonRunner <(echo ASTParserLoadingTest) 0`):

- Reached and **passed** the exact same point (`Marelo.hbm.xml` deprecation
  warning) within the first ~45s, then kept producing new, different log
  content for the next 4+ minutes — DDL schema export for ~25 distinct global
  temp tables (`HTE_Animal`, `HTE_Mammal`, `HTE_DomesticAnimal`, `HTE_Cat`,
  `HTE_Reptile`, `HTE_Lizard`, `HTE_Human`, `HTE_Dog`, `HTE_Joiner`, …),
  followed by actual entity-persister query execution (`select ... from
  "foos" f1_0 join jointable ...`) — clear, continuous forward progress, not
  a repeat of one line.
- CPU-time sampling (`Get-CimInstance`-equivalent `Get-Process` `.CPU`) showed
  near-1-core-saturated, monotonically increasing CPU across the whole
  window (e.g. 31s→110s→183s→269s→294s CPU accumulated over successive
  ~30-45s wall-clock intervals) — essentially 1:1 with wall time, i.e.
  compute-bound, not idle/parked.
- The internal `--stack-dump-on-timeout 300` watchdog fired at the 300s mark
  and, before aborting the process, took a rapid burst of **3911 successive
  stack samples of the single `main` thread**. Frame count fluctuated
  sample-to-sample (131, 132, 135, 137, …) and the leaf frames differed
  between samples (`ClassInfoImpl.hasFactoryConstructor` →
  `RuntimeInlineAnnotationReader.getClassValue` → `XmlTypeQuick.factoryClass`
  in one sample; different call shapes in others) — definitive proof this is
  **not** a frozen/parked thread at a fixed PC; it is genuinely executing,
  just very slowly. Tabulating `class=` across all depth-120..139 frames in
  the whole dump burst: the overwhelming majority are JAXB's
  `RuntimeModelBuilder`/`ModelBuilder`/`ClassInfoImpl`/`RegistryInfoImpl`/
  `RuntimeClassInfoImpl`/`ReflectionNavigator` — i.e. the entire observed
  window (from `@@BEGIN` through the 300s abort) never escaped JAXB's
  reflection-based `@XmlType`/getter-setter/factory-constructor model
  construction.

**Conclusion: real, severe throughput problem — same mechanism as the
existing `update_root_snapshot` reflection-storm wall, not a new hang
class.** The interpreter's per-native-call GC-root-snapshot rebuild is
`O(current interpreter stack depth)` (see
[`reference_update_root_snapshot_reflective_chain_scaling_20260721`] and
[`docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`](../../internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md)),
and JAXB's reflection-heavy `getDeclaredFields`/`Method.invoke`-based model
walk for a 130+-frame-deep, many-annotated-class hierarchy is exactly the
"class-init/reflection storm churns the top frames every call" shape that
doc identifies as the worst case for that mechanism (its own worst
previously-recorded number, Tomcat's `TestApplicationFilterConfig` deploy,
was ~29s / ~15x HotSpot — **this class's bootstrap alone hadn't finished
after 300s+**, i.e. this is a substantially worse-than-previously-documented
instance of the same root cause). The `04-...OPEN.md` doc itself already
records a 2026-07-21 cross-confirmation from an *independent* Spring Boot
JUnit5 investigation (`Jackson`/`OAuth2ResourceServer` "severe slowdown, not
a hang" — same day as this run); this Hibernate/JAXB finding is a third,
independent trigger of the identical mechanism, this time via
`org.glassfish.jaxb.runtime.v2.model.impl.*` reflection rather than Tomcat's
Digester or Spring's bean-creation reflection.

## Not a new bug — do not duplicate the fix effort

An active, unmerged worktree (`C:/craton/CratonVM-oauth2-rootsnapshot-20260721-019f86d0`,
branch `codex/fix-oauth2-rootsnapshot-20260721-019f86d0`) is, as of this same
day, already working the identical `update_root_snapshot` mechanism from the
Spring/OAuth2 angle (uncommitted changes touching `vm/src/vm/vm_exec.rs`,
`vm/src/runtime/env_cache.rs`, `vm/src/runtime/instrument.rs`,
`classloading/src/class_manager.rs`, `native-builtins/src/lang_class.rs`).
**No fix attempted here** — this doc exists only to record that
`ASTParserLoadingTest` is a confirmed, severe, additional trigger class for
that same root cause, worth re-checking once that effort lands.

## Host-load caveat

This solo repro ran on a heavily shared host: at the time of the 300s abort,
`Get-Process cratonvm` showed **7-9 concurrent CratonVM processes** from
unrelated sessions (visible CPU hogs included an unrelated
`cratonvm-spring-boot-suite` process and several other `cratonvm.exe`
instances). The qualitative finding (continuous, varying-depth, JAXB-only
forward progress, never stuck) is unaffected by contention, but the absolute
300s-and-counting figure is almost certainly inflated versus a quiet host —
this class might complete meaningfully faster in isolation, though given the
depth/complexity of its domain model it would still likely exceed the
harness's flat 300s default even on a quiet host.

## Recommendation

Not independently fixable here (same root cause as the tracked, deferred,
high-risk `update_root_snapshot` safepoint-gated-publish redesign). Re-run
this class once the `oauth2-rootsnapshot` fix effort (or any
`update_root_snapshot` fix) lands on `dev`, using
`--stack-dump-on-timeout 60` and a `--timeout` well above 300s, to confirm
whether it closes. Until then, treat this class's `HANG` status in the
suite runner as expected/explained, not a regression — same posture as this
run's `DefaultCatalogAndSchemaTest`/`NamespaceTest` findings (see
[`qualfiedtablenaming-hang-cluster-20260721.md`](qualfiedtablenaming-hang-cluster-20260721.md)).
