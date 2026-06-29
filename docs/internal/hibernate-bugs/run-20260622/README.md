# CratonVM — Hibernate ORM full-suite bug run, 2026-06-22/23

**Binary:** `cvhibtest.exe` (release, dev `c863b23e`, worktree `C:/craton/CratonVM-hibtest`, branch `chore/hibernate-fullsuite-20260622`)
**Harness:** `.cratonvm-suite` fork-per-class JUnit5 runner, 4533 test classes, hang timeout **300 s/class**
**Baseline:** HotSpot JDK 25 (`.cratonvm-suite/out-hotspot-hs`, 4052 PASS)
**Configs run:** default (JIT-on) `out-cratonvm-dev20260622` · `--nojit` `out-cratonvm-nojitfull`

---

## RETEST on updated dev `f8cdd52b` (2026-06-23, binary `cvhib2.exe`, +95 commits)

**FIXED (2):**
- **HIB-CV-20/21 — JIT hang / silent wrong-result / SIGSEGV** ✅ — fixed by `d53c0e96 fix(jit): flip precise JIT oop maps to default-OFF`. `MinSeq` under JIT now parses 13/13; `EntityTest` under JIT PASSES 10/10 in 11.6 s (was >300 s hang). This was ~99% of all failures.
- **HIB-CV-23 — orm.xml mapping hang (`Ejb3Xml*`)** ✅ — `Ejb3XmlOneToOneTest` now PASSES under `--nojit`.

**STILL REPRODUCE on `f8cdd52b` (11 clusters)** — respawned as task chips:
HIB-CV-22, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35. Retest output: `.cratonvm-suite/out-retest2/`.

---

## Headline finding

In the default (JIT-on) config the suite shows a large `HANG`/`FAIL` cluster.
A `--nojit` re-run of the first 104 failing classes shows **103/104 pass without
JIT** — so **~99% of CratonVM-only failures are caused by the JIT**, as a mix of
**infinite-loop hangs** and **silent wrong results**. The interpreter runs
Hibernate correctly. There were **0 genuine CratonVM-only correctness bugs** in
that batch (the lone `--nojit` FAIL is a Mockito harness issue that fails on
HotSpot too).

## FINAL TOTALS

### `--nojit` full run (4533 classes, ~493 min / 8.2 h) — the clean correctness picture
PASS 3902 · FAIL 211 · HANG 36 · CRASH 4 · LOADERR 293 · NOTESTS 85 · ABORTED 2.
Tests executed: **found 13004, ok 11721, failed 331.**

CratonVM-only (HotSpot good) bad results:
- **FAIL 128** = 78 interpreter-slowness timeouts (120 s/test cap) + **50 real correctness FAILs**
- **CRASH 4** (all real CratonVM-only)
- **HANG 35** = mostly interpreter slowness on big query classes + the real `Ejb3Xml` hang
- LOADERR / NOTESTS / shared-FAIL = environmental (fail on HotSpot too) — **not CratonVM bugs**

### default JIT-on run (partial census)
Confirms the JIT is the dominant failure source (bad-list sweep: 103/104 JIT-on failures pass under `--nojit`).

## Ranked recommendation (handoff vs fix)
1. **[HIB-CV-20/21] JIT** — hangs + silent wrong-results + SIGSEGV; ~99% of default-config failures, and the interpreter is too slow without it. **Top priority; hand off to JIT owner.** `MinSeq.java` is a clean repro.
2. **[HIB-CV-32/33] two interpreter SIGSEGVs — DIFFERENT root causes** (verified). **CV-32** = deterministic byte[]→BLOB bind fault (crashes under every GC config). **CV-33** = GC heisenbug: the non-moving young-gen sweep corrupts the live heap under heap pressure (gone with `CRATONVM_DBG_FORCE_MOVING=1` or a big heap); NOT stack-overflow, NOT deterministic. CV-33 → GC owner.
3. **[HIB-CV-25] CDI/Weld** — ✅ **FIXED** (all `cdi.*` green == HotSpot). Two independent native bugs, neither was meta-annotation reflection: (a) `HashSet.containsAll(foreignCollection)` vacuous-true in native-collections poisoned Weld's `SharedObjectCache` set interning → `WELD-001301`; (b) `java.io.DataOutputStream.written` was maintained at the wrong field slot (1 instead of 3) so a subclass's `getfield written` saw 0 → jboss-classfilewriter back-patched offset 0, corrupting class-file magic → `WELD-001524` on every client proxy. Writeups → `docs/internal/hibernate-bugs/HIB-CV-25-cdi-weld-qualifier-containsall-foreign-collection.md` and `…/HIB-CV-25b-weld-clientproxy-dataoutputstream-written-slot.md`.
4. **[HIB-CV-31] `AbstractMethodError` on `FlushEventListener.onFlush`** — **NOT a dispatch defect** (root-caused 2026-06-23). Dispatch is correct; the AME is a mis-attributed cascade: the **BLOB-bind reference corruption of [HIB-CV-32]** makes the BLOB `InputStream` read back as `java/lang/Object` → `NoSuchMethodError: Object.read()I`, which trips `try_lambda_dispatch`'s retry-on-interface fallback → AME. Two fixes: narrow the lambda retry guard; fix the BLOB-bind corruption (CV-32).
5. **[HIB-CV-34] JDBC time-zone offset** — silent wrong data (4 classes).
6. **[HIB-CV-24] classloader isolation/delegation**, **[HIB-CV-29] deserialize-List**, **[HIB-CV-27] in-process javac resources** — subsystem gaps.
7. **[HIB-CV-23] orm.xml mapping hang**, **[HIB-CV-22] TimeoutExtension interceptor**, **[HIB-CV-26] DriverManager**, **[HIB-CV-28] @BatchSize SQL count**, **[HIB-CV-30] cascade null**, **[HIB-CV-35] long-tail**.

---

(Original interim note) A full `--nojit` run surfaced the genuine correctness/feature
bugs across all 4533 classes (those that fail *without* the JIT in play).

## Bugs

| ID | Title | Severity | Status |
|----|-------|----------|--------|
| [HIB-CV-20](HIB-CV-20-jit-xerces-xsd-schema-parse-hang.md) | Foreground JIT miscompile → infinite loop parsing XSDs via Xerces `SchemaFactory.newSchema()` | High | **Root-caused, minimal repro** (`MinSeq.java`) |
| [HIB-CV-21](HIB-CV-21-jit-hangs-hibernate-orm-bootstrap-UMBRELLA.md) | UMBRELLA: JIT hangs, silently miscompiles, **and SIGSEGV-crashes** Hibernate ORM tests; `--nojit` resolves | Critical | Confirmed + quantified (103/104) |
| [HIB-CV-22](HIB-CV-22-junit-timeoutextension-interceptor-double-invoke.md) | JUnit `TimeoutExtension` → "InvocationInterceptors called multiple times" (thread/timeout race) | Medium | Confirmed real (reproduces `--nojit`), intermittent |
| [HIB-CV-23](HIB-CV-23-nojit-hang-orm-xml-mapping-processing.md) | Non-JIT hang processing `orm.xml` mapping fragments (`Ejb3Xml*` family) | High | Confirmed real (`--nojit` hang on test #1); root cause open |
| [HIB-CV-24](HIB-CV-24-classloader-isolation-delegation.md) | Classloader isolation/delegation not honored (supplied `ClassLoader` bypassed; wrong defining loader) | High | Confirmed real, **deterministic** `--nojit`; ties to SBR-14 |
| HIB-CV-25 | CDI/Weld broken — `WELD-001301` on `@Produces` + `WELD-001524` proxy load; all `cdi.*` classes. **Two native bugs (NOT meta-annotation reflection): `HashSet.containsAll(foreign)` vacuous-true + `DataOutputStream.written` wrong field slot.** | High | ✅ **FIXED** (native-collections + native-io); all `cdi.*` green; writeups → [containsAll](../../internal/hibernate-bugs/HIB-CV-25-cdi-weld-qualifier-containsall-foreign-collection.md), [clientproxy](../../internal/hibernate-bugs/HIB-CV-25b-weld-clientproxy-dataoutputstream-written-slot.md) |
| [HIB-CV-26](HIB-CV-26-drivermanager-classforname-registration.md) | `DriverManager` registration via `Class.forName` fails (HHH-7272) | Medium | Confirmed real, **deterministic** `--nojit` |
| [HIB-CV-27](HIB-CV-27-inprocess-javac-message-bundle-broken.md) | In-process Java compiler "compiler message file broken" — `jdk.compiler` resource-bundle not loadable | Med-High | Confirmed real, **deterministic** `--nojit` |
| [HIB-CV-28](HIB-CV-28-entitygraph-batchsize-wrong-sql-count.md) | Entity-graph / `@BatchSize` association fetching issues wrong number of SQL queries | Medium | Confirmed real, **deterministic** `--nojit` (both tests) |
| [HIB-CV-29](HIB-CV-29-deserialize-list-not-in-base-module.md) | Deserialization fails: `StreamCorruptedException: List implementation not in base module` — **NOT CratonVM-internal**; real `Throwable` guard tripped by non-canonical `Class.getModule()` (fresh `Module` per call → identity mismatch). Root-caused + fixed; see [internal doc](../../internal/h2-suite-bugs/run-20260622/HIB-CV-29-getmodule-identity-deserialize-list.md) | High | ✅ **FIXED** (working tree; `--nojit` == HotSpot) |
| [HIB-CV-30](HIB-CV-30-multilevel-cascade-composite-id-null.md) | Multi-level cascade with composite id loses an association (null) | Med-High | Confirmed real, **deterministic** `--nojit` (both variants) |
| [HIB-CV-31](HIB-CV-31-abstractmethoderror-interface-dispatch-no-code.md) | `AbstractMethodError: FlushEventListener.onFlush ... has no Code attribute` — **NOT dispatch**; mis-attributed cascade over BLOB-bind corruption (== CV-32) + `try_lambda_dispatch` retry-on-interface masking. Root-caused; see [internal doc](../../internal/h2-suite-bugs/run-20260622/HIB-CV-31-abstractmethoderror-onflush-root-cause.md) | High | Confirmed real, **deterministic** `--nojit` |
| [HIB-CV-32](HIB-CV-32-sigsegv-blob-bytearray-bind.md) | **SIGSEGV** binding a `byte[]` as a BLOB parameter (JDBC insert) | High | Confirmed real, **deterministic** `--nojit` (hard crash, read fault) |
| [HIB-CV-33](HIB-CV-33-sigsegv-execute-fault-joined-inheritance-sf-build.md) | **SIGSEGV** (execute fault) building a JOINED-inheritance SessionFactory — **GC non-moving young-sweep corrupts live heap under pressure** (NOT stack-overflow, NOT deterministic) | High | **Root-caused** — heisenbug; gone with `CRATONVM_DBG_FORCE_MOVING=1` / big heap; GC owner |
| [HIB-CV-34](HIB-CV-34-jdbc-timezone-timestamp-offset.md) | JDBC time/timestamp **time-zone offset** wrong (silent bad data; 4 classes, fixed 5h/8h shifts) | High | Confirmed real, **deterministic** `--nojit` |
| [HIB-CV-35](HIB-CV-35-longtail-correctness-divergences.md) | Long-tail inventory: sorted-set order, `UnsupportedOperationException`, deserialize/proxy CNFE, stats/stateless state, `.par` URL, non-SIGSEGV crash variants | Mixed | Confirmed real (grouped) |

### Candidates (HS=PASS, not yet deep-dived / entangled)
- `XmlFormatterTest` — `UnsupportedOperationException` (XML formatting API gap?). HS=PASS.
- `DatabaseTimeZoneMultiTenancyTest` — bare `AssertionError` (multi-tenancy/timezone). HS=PASS.
- `NoDepthTests` — `Could not create URL for archive: fetch-depth.par` (.par archive URL). HS=PASS.
- `JarVisitorTest` — recorded CRASH rc=0 (clean exit mid-class). HS=PASS.
- `DynamicMapOneToOneTest` — CRASH rc=127 (non-SIGSEGV abort mid-class). HS=PASS.
- `ProxyClassReuseTest` — `RuntimeException: ClassNotFoundException` during proxy class reuse. HS=PASS.
- `NativeQueryConstructorErrorTest` — `expected:<true> but was:<false>` (native-query error handling). HS=PASS.
- `TransactionTimeoutTest` — bare `AssertionError` (transaction-timeout; timing-sensitive).
- `StoredProcedureResultSetMappingTest` — confirmed **same root as HIB-CV-27** (in-process javac), not separate.

### Candidates still under investigation
- `OneToOneJoinTableUniquenessTest` — intermittent `Could not build SessionFactory: Unable to open specified script target file` (schema-export file-IO) vs the HIB-CV-22 interceptor error; flaky.
- `JarVisitorTest` — recorded CRASH under `--nojit` with rc=0 (process exited mid-class without result); needs confirmation.

### Ruled out by the HotSpot baseline (not CratonVM bugs)
- `FilterImplSerializationTest` — `MockMaker` plugin init (Mockito not configured in harness; **HotSpot fails identically**).
- `BytecodeEnhancement*` / `EmbeddedIdLazyOneToOneCriteriaQueryTest` — `BytecodeEnhancedTestEngine is disabled` (**HotSpot LOADERRs too**).
- `JoinFormulaManyToOneLazyFetching[WithIdClass]Test` — looked like a correctness FAIL but **passes 2/2 under `--nojit`** → it is a JIT silent-wrong-result, folded into HIB-CV-21.

## Repro / evidence files
- `MinSeq.java` — 13-XSD sequential parse, hangs under JIT (primary HIB-CV-20 repro)
- `MinSchema.java` — single-XSD parse control (passes)
- `HangProbe.java` — per-test JUnit progress probe (localizes the stalling method)
- `matrix.txt` — JIT/heap/bg-worker/nojit config matrix for HIB-CV-20

## Method note
Under JIT-on, both `HANG` and non-timeout `FAIL` results are JIT-contaminated.
Genuine CratonVM correctness bugs = failures reproduced **under `--nojit`** and
**not** failing on HotSpot. That is the filter used here.
