# `--jdk-only`: the completion roadmap

> **RE-ADJUDICATED 2026-08-27, fifteen days on, and this page is now STALE in
> three places.** Full evidence and six fixes in
> `docs/known-issues/jdk-only/the-roadmaps-phase-1-and-3-re-adjudicated-and-six-fixes-20260827.md`.
> Read that before starting any lane below.
>
> * **PHASE 3 IS CLOSED.** All four items, 35 probe rows, 0 differing lines in
>   BOTH modes (`apps/probes/Phase3Sweep.java`) — including P3-A, which this page
>   still calls "the one live red in the suite": `aastore` covariance now holds
>   on the compiled tier, checked after 400 000 warming stores.
> * **PHASE 1 IS FIVE-NINTHS CLOSED, AND THE MECHANISM HAS INVERTED.**
>   A/B/D/G/I clear; not one `NoClassDefFoundError` in 80 rows
>   (`apps/probes/Phase1Sweep.java`). What is left is the opposite shape: strict mode
>   has **12** differing lines and COMPATIBLE mode has **18**, every extra one a
>   place where declining to fabricate got the real JDK and the default did not.
>   `apps/probes/AtomicUpdaterSweep.java` is 87/87 clean under `--jdk-only` and DIES
>   in compatible mode.
> * **§5's "`--jdk-only-report` is a complete census and nothing was using it"
>   is no longer true.** `difftest/src/census.rs` and `difftest/src/ledger.rs`
>   consume it today, folding the violation tallies onto the ledger row.
>
> §5's own instrument rules are still exactly right and cost real time to
> re-learn: a dump flag AFTER the main class is silently ignored; the report path
> must be Windows-shaped on this host; and the report is not written when the
> program calls `System.exit`.
>
> The **definition of done** in §6 is untouched by any of this, and is still the
> bar: a Spring Boot application, a servlet container serving HTTPS, and a JDBC
> workload each running to completion under `--jdk-only` with no fabricated class
> instantiated, whatever its package. None of those three workloads is checked
> out on this host — only their runners are — so nothing above should be read as
> evidence about it.

**Status: FINAL. Written 2026-08-12 from measurement, not from record titles.
UPDATED the same day by a second measurement wave that falsified several of this
document's own claims.** Each is corrected in place — what was believed, what
was measured, why the old inference was reasonable — because in this project the
correction is the durable artefact and a silently rewritten number teaches
nobody. Nothing here is deleted for being wrong.

> **PHASE 1 IS BUILT AND MEASURED (2026-08-12 evening).** See
> `docs/known-issues/jdk-only/P1-RESULT-20260812.md`. On a release binary from
> this branch, against HotSpot 25 as oracle: **53 of 54 probe checks now match,
> from 28 of 54**, and `compatibility-class-requested` fell **19 → 6** with all
> six survivors being the recovered-from set. Eight of the nine blocking
> families are closed; the ninth (FFM) turned out to be a **pre-existing
> `Arena.allocateFrom(String)` defect** that the carrier fix merely made
> reachable — the carrier itself matches HotSpot field for field, proved by a
> positive control.
>
> **What that does not license:** 54 deterministic checks are not an
> application. §6's definition of done — a Spring Boot application, a servlet
> container serving HTTPS, and a JDBC workload each running to completion — is
> **unmet and unattempted**. A corpus runner landed this session and **no corpus
> has been run under `--jdk-only`**. Phase 2's 229 measured shadow rows are
> untouched. Read the result document before treating any lane here as done.

The goal, stated so it can be falsified: **any Java application that runs on
HotSpot 25 runs on `cratonvm --jdk-only`, with no synthetic class library
underneath it.** Not "the regression suite is green" — that suite is 72 small
deterministic vectors and it is already at 69 passed / 2 failed in both arms,
which licenses almost nothing about applications.

Three censuses were taken on 2026-08-12 and this plan was derived from them.
Read them before taking a lane; every number below is theirs, not this
document's:

* APP-READINESS-20260812.md — what actually stops a real app, measured by
  running Tomcat, H2 and Spring on this host.
* STUB-CENSUS-20260812.md — the real shape of the native surface, per row, from
  a registry dump against a strict binary.
* RETIREMENT-20260812B.md — what is finished and why, per record.

Four more landed later the same day and are the evidence for the corrections
below. They are in `docs/known-issues/jdk-only/` unless noted:

* P1-BASELINE-20260812.md — the Phase 1 "before", HotSpot-oracled, and the
  measured blocking set: **nine** families, not §1's original three.
* JDK-ONLY-REPORT-CENSUS-20260812.md — `--jdk-only-report` is a complete JSON
  census and nothing was consuming it; also the census-vs-probe rule now in §5.
* P4B-SYNTHETIC-JDK-MODE-20260812.md — why `--synthetic-jdk` mode has not been
  run, and why "run it once" is not a chore that a shipping binary can do.
* `docs/feature-designs/jdk-only-corpus-runner.md` — the corpus inventory as
  *measured by filesystem census*, against what §0 below asserted.

---

## 0. What is already true, so nobody re-derives it

* **Embedded Tomcat 12 boots, serves GET/POST/404, and shuts down cleanly under
  `--jdk-only`.** Measured twice. This is the single most important fact in this
  document: the VM is much closer than the record count suggests.
* A 27-point sweep of the JDK surface real applications use passes **26/27** —
  reflection, annotations, proxies, `MethodHandles`, `URLClassLoader`,
  `ServiceLoader`, executors, locks, `CompletableFuture`, files, NIO, sockets,
  `HttpURLConnection`, `HttpClient`, JCA, XML.
* `--jdk-only` drops **every** `SyntheticStub` and nothing else (1282 → 0).
* Of 9342 natives that survive into strict mode, **only 692 are ones a JVM is
  obliged to provide**. The rest are class-library convenience.
* Category (B) — a stub whose refusal breaks working bytecode — measured
  **zero**. In one case strict is *more* correct than Compatible:
  `Collections.synchronizedList` loses elements in Compatible (6534 and 4697 of
  8000 across two runs) and is exact under `--jdk-only`.

**The corpus is not the constraint.** H2, Tomcat 12, Spring Framework 7.1,
Hibernate, Keycloak, WildFly, Elasticsearch, Kafka, commons-math and bc-java are
all built on this host. What is missing is runner setup, and both existing
runners can be bypassed by composing a classpath by hand from the built output.
The only genuinely absent corpus is Spring Boot.

**Corrected 2026-08-12 by a filesystem census** (`jdk-only-corpus-runner.md`
§2). Spring Boot being the single genuine absence is **right** — but the reason
it reads as wrong is that `cratonvm/apps/spring-boot` *exists*, and contains
only `sb-runner` (two `.java` files), zero jars and zero class directories. It
is a decoy. Generalised, because this host has **nine** such directories: **a
`*-suite-runner` directory is not evidence that a corpus is built.**

Two further claims above assume a directory's existence reports its contents,
and both are wrong in that same direction:

* **Tomcat needs no `ant` build.** `output/classes`, `output/testclasses` and
  `output/build/lib` are already populated. The ~20-minute recompile the brief
  describes is not on the critical path here.
* **Neither H2 root alone composes a working classpath.**
  `apps/h2database/h2/target/classes` holds exactly **one** file
  (`META-INF/versions/21/org/h2/util/Utils21.class`), no `org/h2/Driver.class`,
  and its `target/test-classes` is empty; `cratonvm/apps/h2database/h2` is fully
  built but has **no `ext/` directory**, while the *unbuilt* root carries all 13
  dependency jars. The runner has to union the two, and must resolve on a
  **specific expected artefact** (`target/classes/org/h2/Driver.class` plus
  `target/test-classes/org/h2/test/TestBase.class`), never on directory
  existence. That is not fastidiousness: H2 discovery yields **217** concrete
  test classes, so a runner that picks the decoy runs all 217 against a
  classpath with no `org/h2/Driver.class` and emits one identical
  `NoClassDefFoundError` per class — a wall indistinguishable from a sweeping VM
  linkage regression. Hibernate is the *measured* instance of that shape (after
  the path rewrite, **57 of 241** classpath entries exist and 183 are evicted
  gradle module-cache jars), and the corpus runner refuses with the diagnosis
  rather than produce the wall.

---

## 1. PHASE 1 — the mechanism that blocks applications

**One defect shape, ~~three~~ NINE reachable instances**, all independent lanes.

> **Corrected 2026-08-12 (P1-BASELINE-20260812.md).** This section read "three
> reachable instances — this is the whole of what stops real applications
> today". The mechanism statement below was and is exactly right; the *count*
> was not. Why the inference was reasonable: three is precisely what the
> workloads that had been run — Tomcat, Spring, H2, and a 27-point surface
> sweep — happened to reach, and each was reproduced twice against a HotSpot
> oracle. But **a screen reports its own reach, not the defect.** Two later
> waves found six more: a 33-probe reachability screen driven off the two
> authoritative mint tables (`native-api/src/no_image_receiver.rs`: 49
> `NO_IMAGE_JDK_RECEIVERS` + 9 `VM_MINTED_STAND_IN_RECEIVERS`, i.e. **57**
> candidates, not 3), and then a **source audit** that predicted families no
> probe had a route to. HotSpot is 33/33 and 7/7 across those waves; nothing
> below is an oracle disagreement.

> A native registered in the **essential** set survives strict mode, runs, asks
> for a **fabricated** receiver, gets the refusal that `--jdk-only` exists to
> give, and dies as `NoClassDefFoundError` at the application's call site.
> **The refusal is correct. The survival of its caller is the defect.**

| lane | native | what it takes down |
|---|---|---|
| **P1-A** | `Atomic*FieldUpdater.newUpdater` → fabricated receiver | `java.sql.SQLException` holds a static `AtomicReferenceFieldUpdater`, so **the entire `java.sql` package is unloadable**. All JDBC, all ORM, all connection pools. It also corrupts exception identity far from JDBC: H2's `TestStringUtils` fails with "expected `DbException`, got `NoClassDefFoundError`". |
| **P1-B** | `System.getenv()` (no-arg only) → `cratonvm/internal/UnmodifiableMap` | **Spring dies in `AbstractEnvironment.<init>`**, before bean one. |
| **P1-C** | `SSLSocket.get{Output,Input}Stream()` → fabricated stream | Handshake succeeds, first stream access throws. **No HTTPS.** |

**The six this document did not name**, added 2026-08-12. Same mechanism, same
lane shape; P1-BASELINE has the probe transcripts and the reach argument.

| lane | fabricated receiver / mint site | what it takes down |
|---|---|---|
| **P1-D** | `java/util/function/Consumer$AndThen` and `Predicate$$Lambda$*` — `native-builtins/src/phases_late/streams.rs` | `Consumer.andThen` and `Predicate.and/or/negate`: **default methods on the core functional interfaces**. Streams, collection pipelines, Spring, essentially every modern codebase. `SSLSocket` blocks HTTPS; this blocks ordinary control flow |
| **P1-E** | `java/lang/foreign/DowncallHandle` — `native-builtins/src/panama.rs` | `Linker.downcallHandle`, i.e. **all of Panama/FFM**. netty and the newer high-performance libraries take this road when it is available, and CratonVM pins `sun.misc.unsafe.memory.access=allow`, which changes which road some of them pick |
| **P1-F** | `cratonvm/internal/SnapshotEnumeration` | `ConcurrentHashMap.keys()/elements()` |
| **P1-G** | `java/util/Enumeration$Impl` | `Hashtable.keys()/elements()`. Note this class is *also* one of the 13 the boot block refuses — the boot refusal is not the one that kills the call site |
| **P1-H** | `java/util/concurrent/CompletedFuture` | `AsynchronousFileChannel` read/write — **H2's `async:` filesystem**, i.e. part of the very workload Phase 4 is meant to adjudicate Phase 2 with. The mint site's own comment names `FileAsync.write` and `TestFileSystem.testConcurrent` |
| **P1-I** | `cratonvm/synthetic/Process` — `native-io/src/process.rs::spawn_and_wrap_with_redirects` | `Runtime.exec`, all six overloads. **This row carries its own control:** `ProcessBuilder.start` shares the mint, was re-tagged `SyntheticStub` and **passes**, while the six ambient-`Bridge` `Runtime.exec` overloads fail — the pinned half and its twin, in one probe run. P1-BASELINE records the fix as re-tagging all six; verify before taking the lane |

**Two shapes, not one — a lane that repairs only class-minting leaves half of
P1-D red.** `Consumer$AndThen` is the fabricated-receiver refusal (P1's shape).
`Predicate.test` fails as `AbstractMethodError … has no Code attribute` — the
fabricated object exists and its method *body* does not. And the family is
half-fixed already: `Function.identity/andThen/compose` **passes** from the same
file, minted the same way, which is exactly why a green `Function` check is not
evidence about `Predicate`.

**Provenance of the nine, because it is the argument for using both
instruments:** five found by probing, three by a source audit, and one
(`java/util/Enumeration$Impl`) by the probe written to test one of the audit's
*other* predictions.

**How the audit's three were found, stated because the method is reusable.** It
predicted them from a two-term rule that needs no run: a mint is a
live `--jdk-only` blocker iff **(1)** the receiver is on
`NO_IMAGE_JDK_RECEIVERS`/`VM_MINTED_STAND_IN_RECEIVERS` *and* **(2)** the
minting native is `bridge` — it survives strict mode and asks anyway. Term 2 is
free: `scripts/baselines/jdk-only-kind-map-25-linux.tsv` is a frozen
per-registration census with an adjudicated kind per triple. The audit's own
scoping was wrong where the probe was right — `Properties.propertyNames()` was
predicted the widest failure and **passes** — which is the §5 rule in miniature:
neither instrument's verdict survives without the other's.

Also settled by that audit: `sun/misc/Cleaner` and
`jdk/internal/logger/AbstractLoggerFinder` have **zero** registrations in the
kind map. They are stale table rows, not live fabrications; a reader counting
fabricated classes off the table overcounts by two.

Two-line witness, reproduced twice per arm — `Class.forName("java.sql.SQLException")`
and `System.getenv()` both fail under `--jdk-only` and both pass on HotSpot.

**Attribution is clean and this is pre-existing, not campaign damage:** the
pristine `44044c7e2` control fails the JDBC probe identically, same stack.

**Prescriptions, each measurable on its own:**

* **P1-A** — either de-register the native under `JdkOnly` and let the real
  `AtomicReferenceFieldUpdaterImpl` run, or return a real object. The
  de-registration may resurrect the `ClassCastException` the native was written
  for: **measure that first**, do not assume either way.
* **P1-B** — the native already builds a real `HashMap`. Call the real
  `Collections.unmodifiableMap` on it instead of the fabricated wrapper. Note
  that `vm_init.rs::ensure_bootstrap_compat_class`'s premise — "these stand-ins
  exist for the synthetic collection shims, which strict mode does not
  register" — is **false for `UnmodifiableMap`**: `wrap_system_env_map` uses it
  and ships in the essential set.
* **P1-C** — do the strict-mode receiver work **separately from** async-close.
  The async-close fix is unsound as designed (see §3) and must not be bundled
  with this.
* **P1-D … P1-I** — no prescription is written here, deliberately: each mint
  site is named in the table above and the diagnosis is in
  P1-BASELINE-20260812.md, which is where a prescription should be derived
  from rather than guessed at from a lane letter. Two constraints that do
  generalise. **(1)** P1-I's shape is the standing one — the pinned half
  (`ProcessBuilder.start`, re-tagged `SyntheticStub`) passes while its ambient-
  `Bridge` twin fails from the *same* mint, so before declaring any of these
  fixed, find the twin and check it in the same run. **(2)** P1-D is two
  defects wearing one name; repairing the class-minting leaves `Predicate` red
  on `AbstractMethodError … has no Code attribute`, which is a missing method
  body, not a refused class.

**Cross-cutting, cheap, do it first:** emit the refusal `warn!` at *every*
`try_ensure_synthetic_class`, not only the boot block.

> **Corrected 2026-08-12.** This said the startup banner "under-reports the
> blocker set by half — 13 of 27". The arithmetic was sound and the framing was
> not. That ratio came from six Compatible-mode census runs; measured against
> `--explain-jdk-only` on a strict binary the split is **categorical, not
> fractional: every boot-time refusal is reported and every runtime refusal is
> silent.** Exactly 13 are printed, all from the boot block in
> `vm/src/vm/vm_init.rs`, all before `main`, and the message text is good — it
> names the class, says the natives bound to it are unreachable, and predicts
> the call-site failure. **None** of the nine runtime families above appears
> anywhere in that output, though each kills an application path in the same
> run. Why this matters more than the number: "half" licenses aiming the fix
> anywhere, whereas the true shape says the boot block is already correct and
> **the entire value of this change is on the runtime path**.
>
> One constraint on the fix: `cratonvm/internal/UnmodifiableMap` is refused at
> boot *and* again at runtime for `System.getenv()`. De-duplicating by class
> name suppresses the second — which is the one that explains an application
> failure. They are different events.

**Exit criterion for Phase 1:** all nine families clear, each by the ordinary
Java route that reaches it — `Class.forName("java.sql.SQLException")`,
`System.getenv()`, an `SSLSocket` stream read, `Consumer.andThen` /
`Predicate.and`, `Linker.downcallHandle`, `ConcurrentHashMap.keys()`,
`Hashtable.keys()`, an `AsynchronousFileChannel` read/write and
`Runtime.exec(String[])` — plus a JDBC probe at 10/10 and a Spring context that
constructs. The standing screen is §6's: the refused-class set, whatever the
package, **not** a `cratonvm/internal/` prefix match.

---

## 2. PHASE 2 — remove the stubs

The target is **not** the 1282 `SyntheticStub` natives; strict mode already drops
all of them and nothing breaks. The target is the population that *runs* in
strict mode and shadows real JDK bytecode:

| category | count | disposition |
|---|---|---|
| runs in strict, **bridge** | **3956** | the work |
| runs in strict, **intrinsic** | 499 | mostly legitimate acceleration (`Math`, `StringLatin1`) — **not roadmap work** |
| fabricated methods on real classes | 335 | delete; all tested families green without them |
| intercepts inherited/abstract declarations | 2865 | audit, lower priority |
| synthetic-jdk only | ~4390 call sites | invisible to both shipping binaries |

**What makes this parallel:** `registry.rs:5782` re-tags a Bridge to
`SyntheticStub` from a **central table**, so retiring a shadow needs no registrar
edit. One serialised lane (**P2-L0**) owns that table and receives nominations;
every other lane nominates and never edits it.

15 lanes, disjoint by registrar file, are enumerated in STUB-CENSUS-20260812.md
§5. P2-L1 (the 335 dead fabricated methods) and P2-L7 through P2-L13 can start
simultaneously today. P2-L5, P2-L6 and P2-L14 own the two largest files and must
run last and alone.

**Two immovables, named so nobody spends a lane on them:** `Object.<init>`
(1399 calls in one probe run) and the `Enum` family are object-layout concerns,
not class-library convenience.

**The honest limit:** proving each of the 3956 bridges *correct* is 3956
differential tests. The tractable form is to retire by family and let the corpus
adjudicate, which is why Phase 2 depends on Phase 4's corpus being wired first.

**Corrected 2026-08-12: the target is measurable, and the instrument was already
shipping** (JDK-ONLY-REPORT-CENSUS-20260812.md). Every count in the table above
is *derived statically* from a registry dump. But the VM reports the same
population per run, attributed to a source line:

```
cratonvm --jdk-only --explain-jdk-only --jdk-only-report r.json -cp <cp> <Main>
```

On one ordinary Java program that emits 1569 violations in three kinds — 19
`compatibility-class-requested`, **226 `native-shadows-bytecode`**, 1324
`synthetic-native-registered` — each row carrying `class`, `requester`
(`file:line`), `initiating_loader` and `reason`, under `schema_version: 1`. It
is meant to be consumed and nothing was consuming it. **The 226 are Phase 2's
target on the path a real program actually took**, which is a better instrument
than 3956 for choosing what to retire first: 3956 is the population, 226 is what
one workload meets, and a family retired without moving a row on any workload
has been adjudicated by nobody. The 226 are not 226 defects either — they are
226 places to *ask* the question, and the corpus is what answers it. Use the
number; do not estimate it.

---

## 3. PHASE 3 — correctness gaps no stub census can see

These are invisible to every native census because they are not natives. Each is
an independent lane.

* **P3-A — the JIT omits the `aastore` covariance check.** `jit_aastore` has
  **no caller on any backend**; the JIT lowers `aastore` inline. Measured
  `cold=[java.lang.Integer] hot=[no-throw]` — a `String[]` slot accepting an
  `Integer` on the compiled tier only. Patch written in full in W7-37 Part 4,
  deliberately unapplied: it puts a Rust-boundary call on the hottest
  reference-store path in the VM and forces `has_dispatch` on nearly every
  compiled method. **Needs a store-heavy A/B, and the perf-preserving form is a
  per-site monomorphic inline cache on `(class_id_of(array), class_id_of(val))`.**
  This is the one live red in the suite (`RExceptions`).
* **P3-B — class-file parsing gap.** `MethodHandleProxies.asInterfaceInstance`
  fails with `ClassFormatError: ldc: unsupported constant pool entry type at #26`.
  Not a stub; a decoder gap, and a hard blocker for anything using it.
* **P3-C — typed linkage errors are flattened.** `native_classloader_define_class1`
  ends every failure with `define_class_format_error(...)`, so a preview class
  file yields `ClassFormatError` carrying a Rust `Debug` string
  (`Linkage(UnsupportedClassVersionError { class_name: "", … })`) where HotSpot
  throws `UnsupportedClassVersionError`. A container catching the JDK type does
  not catch ours.
* **P3-D — async close on TLS streams.** Four sites. **The obvious design is
  unsound**: socket readiness is not stream readiness, and a readiness gate
  deadlocks the ordinary HTTP-over-TLS shape because `rustls`' `wants_read()` is
  false while decrypted plaintext is buffered. The screen must be asked under
  the stream mutex (`conn.wants_read()` / `TlsStream::buffered_read_size()`)
  before releasing it. Pilot on `rustls_stream_read`'s client arm only;
  `*_write` must get no loop.
* **P3-E — `String.format` with no `Locale`** localises against ROOT instead of
  the FORMAT default (W7-91 §5).

---

## 4. PHASE 4 — the evidence base, which gates everything above

**The suite cannot see application defects, and Phase 2 cannot be adjudicated
without a corpus.** Three lanes, all independent. **P4-A and P4-C are startable
now; P4-B is not a run and needs its own build first** — see its correction.

* **P4-A — wire the corpora that are already on disk.** H2, Tomcat, Spring
  Framework, Hibernate, Keycloak, WildFly, Elasticsearch, Kafka are built here;
  the runners want `mvn`/`ant`. Compose classpaths from built output instead.
  Spring Boot is the one genuine absence — but read §0's correction on what
  "absent" and "built" actually looked like on disk before wiring anything, and
  **check the tree before starting**: a corpus runner has since been written
  (`regression-suite/corpus/`, designed in
  `docs/feature-designs/jdk-only-corpus-runner.md`, six corpora wired, four
  inventoried, Hibernate blocked on a gradle resolve). Its own §6 residual is
  the one that matters to this roadmap: **no corpus has yet been run under
  `--jdk-only`**, which is the mode this campaign is named for.
* **P4-B — run `--synthetic-jdk` MODE.** Several records' residuals live only in
  that configuration and cannot be adjudicated any other way. **This is not a
  chore and it is not one run** — see the correction below before scheduling it.

  > **Corrected 2026-08-12 (P4B-SYNTHETIC-JDK-MODE-20260812.md).** This lane
  > said the mode "has never been executed, ever". Too strong, and falsified:
  > `apps/h2database-suite-runner/RESULTS-20260721.md:91-95` records a
  > synthetic-mode boot — it died immediately in `TestBase.<clinit>` on a
  > missing `DateTimeFormatter.ofPattern` stub, which is a *result*, not an
  > absence of one — and **four** in-tree runners pass the flag today
  > (`h2database-`, `spring-`, `hib-` and `tomcat-suite-runner`). The
  > defensible restatement: **the `--features synthetic-jdk` binary has never
  > been run in that mode, and no `RJdk*` vector ever has.** The operative
  > consequence is unchanged — nothing *gating* launches the mode: not
  > `regression-suite/`, not CI, not `scripts/`.
  >
  > **And a harder fact, measured: a shipping binary refuses `--synthetic-jdk`
  > outright, exit 1**, during argument parsing, because none of the ~5,200
  > synthetic stubs are compiled into it (that figure is the VM's own, from the
  > refusal text, and is worth reconciling against the census numbers — which
  > come from a binary in which none of them exist). So the standing "feature ≠
  > mode" note is true but **incomplete: the mode *requires* the feature.**
  > They are not independent axes. Consequences:
  >
  > * There is no "run it once" — it needs its own build of its own binary,
  >   from a clean `git archive HEAD` export rather than a half-edited campaign
  >   working tree. That, not oversight, is why it has never happened.
  > * **A residual living only in that mode cannot be adjudicated by any run of
  >   a shipping binary** — not by the suite at any `SUITE=` value, not by a
  >   corpus run, not by a census. A lane reporting such a residual as
  >   "unreproducible" has measured the wrong binary.
  > * Symmetrically, a defect existing only there cannot affect any shipped run,
  >   so it is a documentation and dead-code concern. Rank those low for *that*
  >   stated reason — not by leaving them silently untested.
* **P4-C — compile the Linux arms.** `native-io/src/process.rs`'s non-Windows
  arms have never been compiled by any lane that edited them, and two ratchets
  plus the kind map are keyed `25/linux` so only a Linux run can re-freeze them.
  This also unblocks the `java/io/Print*` retirement and the
  `BootLoader.loadLibrary` arming, whose A/B **must** run on Linux — the Windows
  road is inert and a green Windows A/B measures the wrong road.

---

## 5. Instrument rules, paid for the hard way

* **The registry census works and the flag order matters.**
  `cratonvm --jdk-only --explain-jdk-only --dump-native-registry census.json -cp <cp> <Main>`.
  Placed **after** the main class the flag is silently ignored — no file, no
  warning, exit 0. `--explain-jdk-only` adds `image_declaring_method`, the
  adjudication against the class-path bytes; without it there is no four-way
  split.
* **A request is not a failure, and this is the rule that makes the census
  usable.** `java/util/HashMap$KeyItr`, `java/util/Comparator$Native` and
  `java/util/Enumeration$Impl` are all requested **and refused** in runs where
  `HashMap` iteration, the `Comparator` combinators and `Collections.enumeration`
  **pass**. The native asks, is correctly refused, and the caller recovers onto
  real JDK bytecode: that is strict mode working as designed. So the two
  instruments have opposite biases and are sound only together — **the census
  over-reports** (reading it alone puts all 19 fabrication requests on the
  Phase 1 worklist when a handful break anything) and **a probe under-reports**
  (it sees only the routes it thought to take, so it can never prove absence).
  **The blocking set is the intersection: refused *and* not recovered from.**
  This is not theoretical here — both waves of the 2026-08-12 campaign found
  families the other could not. Three of the nine Phase 1 families came from a
  source audit no probe had a route to, and the audit's own scoping was wrong
  exactly where the probe was right (`Properties.propertyNames()`, predicted the
  widest failure, **passes**). Neither method's verdict survives without the
  other's.
* **`--jdk-only-report` is a complete census and nothing was using it.** See §2:
  1569 rows in three kinds, `schema_version: 1`, every row attributed to a
  `requester` `file:line`. Two traps beyond the flag-order one above: it needs a
  **Windows-shaped path** on this host (given `/c/Users/...` it prints
  `os error 3` and continues, and the file silently does not appear), and it is
  **not written when the program calls `System.exit`** — probes here grew a
  `-Dprobe.noexit=1` escape so the census could be taken at all.
* **Do not read a pipeline's own behaviour as the subject's.** I reported the
  shutdown `[jdk-only:shutdown] policy violation` report as "naming one
  requested class" and apparently truncated, and filed it as an open question.
  **That was my own `head -40`.** The run emits 1570 lines and the report is
  complete. This is the **third** instance in this campaign of the same species:
  a successful 25-minute build read as "failed" because the command ended in
  `grep -c`; a census claim whose `exit=0` was grep's status; and this one.
  Before attributing a truncation, a silence or an exit code to the VM, check
  what the last stage of your own pipeline did to it.
* **"I cannot derive your claim from the source" is evidence.** The lane handed
  the truncation claim above could not explain it — its reading said all 13
  should print — so it said so and attached the decisive check. That check is
  what resolved the question, against me. Treat an inability to confirm as a
  signal about the claim, not as a lane failing to find something.
* **`apps/probes/` is never run by `run.sh` at any `SUITE=` value.** A record whose
  only evidence is a probe cannot be discharged by a suite run, however green.
* **`TIMEOUT=420` on this host** — `RMapGcStress` needs ~4m55s and times out at
  the 120s default, which then manufactures two `HARNESS ERROR` rows that read
  as independent defects.
* **A harness row under a failing vector is usually downstream of it.** A vector
  that dies emits no output, so the extract and count guards both fire.
* **Never freeze a number you cannot derive.** A firing gate is loud; a wrongly
  frozen one is silent forever. The stub ratchet's own text says so.
* **Non-null is not the contract.** Two defects survived this year behind
  `!= null` and count checks: three fabricated enum constants passed
  `values().length == 3`, and `Thread$State` passed non-null-and-named while
  `values()[0] == State.NEW` was false. Assert identity and equality.
* **Registration is the gate, and `NativeKind` is ambient.** Before changing a
  native, grep every registration of the triple, decide which wins on which boot
  path, and check the ambient kind of the winning block — and separately whether
  that registrar is reached at all in the mode you care about. A `Bridge`
  registrar behind `#[cfg(feature = "synthetic-jdk")]` is as dead in a shipping
  build as a `SyntheticStub` is in strict mode, and nothing refuses it loudly.

---

## 6. Order, and what can run at once

```
NOW, fully parallel:  P1-A  P1-B  P1-C  P1-D  P1-E  P1-F  P1-G  P1-H  P1-I
                      P4-A  P4-C  P3-B  P3-C
                      + the try_ensure_synthetic_class warn! (one line)
                        — aim it at the RUNTIME path; the boot block is done
AFTER P4-A lands:     P2-L1, P2-L7..L13  (parallel)      P3-A (needs the A/B)
                      P2-L0 serialised, receiving nominations from all
LAST, alone:          P2-L5  P2-L6  P2-L14   (the two largest registrar files)
DESIGN FIRST:         P3-D   (the obvious fix is unsound — do not bundle it)
NEEDS ITS OWN BUILD:  P4-B   (a --features synthetic-jdk binary; not a run)
```

P1-D and P1-E are separate lanes from each other and from P1-A/B/C: different
files (`phases_late/streams.rs`, `panama.rs`), no shared call path. P1-B and
P1-F/P1-G touch the same fabricated-collection neighbourhood and should compare
notes before both edit `vm_init.rs`'s boot block.

**Definition of done.** Not a suite number. A Spring Boot application, a servlet
container serving HTTPS, and a JDBC workload each run to completion under
`--jdk-only` with **no fabricated class instantiated, whatever its package** —
screened against the refused-class set the VM reports, not against a prefix.

> **Corrected 2026-08-12.** This read "with no `cratonvm/internal/*` class
> instantiated". **Six of the nine fabricated classes in §1 do not match that
> prefix**: `java/util/concurrent/atomic/…$RustJvmImpl`,
> `javax/net/ssl/SSLSocket{Input,Output}Stream`,
> `java/util/function/Consumer$AndThen`, `java/lang/foreign/DowncallHandle`,
> `java/util/Enumeration$Impl` and `java/util/concurrent/CompletedFuture` all
> live in the JDK's own namespaces. A prefix screen would have reported Phase 1
> clean with every one of them still fabricated. The inference was reasonable —
> `cratonvm/internal/` *is* where most of them live, and `$RustJvmImpl` looks
> like a naming convention — but a convention is not a boundary, and whether
> `$RustJvmImpl` is a family at all is worth establishing on its own. The screen
> is the set of class names refused, wherever they sit.
>
> **And the screening instrument is not `--explain-jdk-only` yet.** By §1's
> other correction that banner reports boot refusals and *only* boot refusals,
> so it cannot see any of the nine runtime families. Until the runtime `warn!`
> lands, verify with `--jdk-only-report`'s `compatibility-class-requested`
> rows, which do cover the runtime path — and read them with §5's rule in hand:
> a request is not a failure, so the criterion is a refusal the caller did not
> recover from, not the presence of a row.
