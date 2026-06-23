# HIB-CV-21 — UMBRELLA: JIT hangs Hibernate ORM tests; `--nojit` resolves them

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`, branch `chore/hibernate-fullsuite-20260622`)
**Severity:** Critical — dominant cause of suite `HANG`/`TimeoutException` results in the **default (JIT-on) configuration**
**Status:** Confirmed; one instance root-caused (see [HIB-CV-20](HIB-CV-20-jit-xerces-xsd-schema-parse-hang.md)); scope quantification in progress

---

## Headline

The large `HANG` cluster in this run is **not raw interpreter slowness** as first
suspected. It is the **JIT**. Classes that `HANG` (>300 s) under the default
configuration complete **cleanly and quickly under `--nojit`**:

| Class | default (JIT on) | `--nojit` | HotSpot |
|---|---|---|---|
| `org.hibernate.orm.test.annotations.EntityTest` | **HANG >300 s** | **PASS 10/10** (<130 s) | PASS 10/10, 10.3 s |
| `org.hibernate.orm.test.annotations.cid.CompositeIdTest` | **HANG** | **PASS 14/14** | PASS |
| `org.hibernate.orm.test.actionqueue.CustomAfterCompletionTest` | **HANG** | **PASS 3/3** | PASS |

The earlier "≈30-40× slowness" reading was partly a misattribution: some classes
*are* slow, but the ones recorded as `HANG` are mostly **JIT deadlock/infinite-loop**,
which `--nojit` removes entirely.

---

## What is confirmed

1. A **foreground JIT miscompile produces an infinite loop** while Xerces parses
   XSD schemas during Hibernate's XML/mapping bootstrap — fully root-caused with a
   Hibernate-free minimal repro in **[HIB-CV-20](HIB-CV-20-jit-xerces-xsd-schema-parse-hang.md)**
   (`MinSeq.java`: parse 13 ORM XSDs → hangs at the 5th–6th under JIT, completes
   under `--nojit`; not heap, not GC, not the background-compile worker).

2. The hang reproduces on real test classes whose `@SessionFactory`/`@DomainModel`
   bootstrap exercises the same hot paths, and `--nojit` flips them green.

## Quantified scope (bad-list `--nojit` sweep)

Re-ran the **104** classes that were `HANG`/`FAIL` under default JIT (from the
first ~260 of the suite) with `--nojit`:

| `--nojit` outcome | count | meaning |
|---|---|---|
| **PASS** | **103** | JIT-induced (infinite-loop hang **or** silent wrong-result) — disappears without JIT |
| FAIL | 1 | `FilterImplSerializationTest` — Mockito MockMaker; **HotSpot fails identically** → environmental, not CratonVM |
| HANG | 0 | none were genuine slowness |

**→ ~99% of CratonVM-only failures in default config are the JIT. Zero genuine
CratonVM-only correctness bugs in this batch.** The interpreter runs Hibernate
correctly.

### The JIT has three failure manifestations

1. **Infinite-loop hang** (most common) — e.g. XSD parsing (HIB-CV-20).
2. **Silent wrong result** — e.g. `JoinFormulaManyToOneLazyFetchingTest` asserts a
   wrong query value under JIT, PASS 2/2 under `--nojit`.
3. **Hard SIGSEGV (rc=139)** — e.g. `org.hibernate.orm.test.boot.models.annotation.SimpleAnnotationUsageTests`
   crashes the process under JIT, **PASS 1/1 under `--nojit`**. No Rust panic — a
   bare access violation from miscompiled code.

### The JIT produces silent wrong results too, not just hangs

`JoinFormulaManyToOneLazyFetchingTest` / `...WithIdClassTest` were recorded as
`AssertionFailedError` (wrong query result) under JIT, but **PASS 2/2 under
`--nojit`**. So the JIT miscompile manifests as **both** infinite-loop hangs and
**silent incorrect computed values** — the latter is the more dangerous class
(data corruption with no error).

## What is still open

- Whether **all** JIT-on hangs share the single HIB-CV-20 root cause, or there are
  several distinct JIT miscompiles on Hibernate's hot paths. The fact that
  annotation-only tests (no XML mapping) also hang under JIT suggests at least one
  *additional* hot-path JIT hang beyond XSD parsing — to be confirmed by
  `CRATONVM_DBG_JITC` on a non-XML hanging class.
- Exact count of JIT-attributable HANG/FAIL classes — being measured by re-running
  the failing-class list under `--nojit` (`nojit-sweep`).

---

## Reproduce

```
# hangs (default):
cvhibtest.exe --java-home <jdk25> @common.args HangProbe org.hibernate.orm.test.annotations.EntityTest
# passes (jit off):
cvhibtest.exe --java-home <jdk25> --nojit @common.args HangProbe org.hibernate.orm.test.annotations.EntityTest
```

`HangProbe.java` (attached) prints `@@START`/`@@END` per test so the stall point is
visible.

---

## Triage recommendation

- **This is the #1 bug to fix** for Hibernate (and almost certainly other
  real-world apps): the default-on JIT infinite-loops on common library hot paths
  (Xerces XSD parsing confirmed). Fixing the HIB-CV-20 miscompile likely unblocks a
  large fraction of the suite in one shot.
- Short-term workaround to get a correctness baseline / unblock app bring-up:
  run with `--nojit`.
- Hand-off vs fix: the JIT miscompile is deep VM work (JIT codegen) — a strong
  **hand-off-to-a-JIT-owner** candidate, with HIB-CV-20's `MinSeq` repro as the
  entry point.
