# Hibernate suite — non-deterministic SIGSEGV cluster == guarded-inline-getfield JIT regression (2026-07-10) — RESOLVED

| | |
|---|---|
| **Status** | GREEN RESOLVED 2026-07-10 on `dev` default builds. The "20/33 classes SIGSEGV" Hibernate cluster is **not** a global-temp-table race — it is the **guarded-inline JIT `getfield` regression** already fixed by `93b33576` (flip to opt-in). Verified by A/B on a dev-HEAD binary + a fresh gdb backtrace. |
| **Real root cause** | Guarded-inline JIT `getfield` fast path (introduced by `07dfa5e0`, merged via `756c4b84`; per the bisection recorded in `93b33576`). Under JIT it can produce a **corrupted `getfield` result — a small integer such as `0x40`** — that is later used as a pointer/return address, giving a SIGSEGV at a tiny address. Suite-agnostic; the Hibernate temp-table classes were collateral, not causal. |
| **Fix (already on dev)** | `93b33576` "Fix guarded-inline-getfield SIGSEGV regression masking ES IVF-KNN vector hang cluster" flipped `guarded_inline_getfield_enabled()` in `jit/src/x64.rs` from **default-ON back to opt-in** (`CRATONVM_JIT_GUARDED_GETFIELD=1`). On `dev` default the crash is gone. The underlying x64 codegen defect in that fast path is **latent** (opt-in only) and not yet pinned to the exact instruction — tracked by `93b33576`s own doc comment for A/B. |
| **Supersedes** | `docs/known-issues/hib-global-temptable-nondeterministic-sigsegv-20260710.md` (`562c1de5`, OPEN, "likely a global-temp-table race" hypothesis) — that hypothesis is REFUTED here. |

## Why the original doc mis-attributed it

The known-issue doc (`562c1de5`) was written against `dev` at `03fd1788` (2026-07-10 21:49). At that commit the guarded-inline `getfield` fast path was still **default-ON**. The very next relevant commit, `93b33576` (21:56, only 7 minutes later), flipped it back OFF — but it landed while investigating an **Elasticsearch** IVF-KNN hang cluster and was never cross-referenced to this Hibernate cluster. So the Hibernate doc captured a real crash whose fix already existed minutes later under a different suites banner.

The "PASS, PASS, HANG, CRASH across 4 attempts" non-determinism reported in the original doc conflated **two independent phenomena**:
- **SIGSEGV (rc=139)** = the guarded-getfield regression. On the doc-era default-ON build, whether a given run crashed depended on which `getfield` site JIT-compiled first and host-load-driven timing of reaching it — hence flaky. With the fast path force-enabled it is effectively **deterministic** (see A/B below).
- **HANG (rc=124)** = the *pre-existing, separately-documented environmental hang* for `DefaultCatalogAndSchemaTest` (HotSpot hangs on it too; see `docs/internal/hibernate-bugs/hibernate-hang-clusters-summary.md` "Environmental (NOT a CV-only bug)"). Unrelated to this SIGSEGV.

`query.hql.FunctionTests`, also on the affected list, has its own prior REFUTED history (`docs/internal/hib-query-hql-functiontests-translation-cluster-NOT-A-BUG.md`) — further evidence the affected-class list is a mixed bag of unrelated symptoms, not a single temp-table bug.

## Verification (2026-07-10)

Worktree `verify/hib-globaltemptbl-sigsegv-20260710` off `origin/dev` (built at `d6058540`, which includes `93b33576`). Binary `cratonvm-hibtmptbl`, real JDK 25 (`--java-home /home/victor/jdk25`), JIT on. The dev-HEAD default build has the fast path OFF; re-enabling it with `CRATONVM_JIT_GUARDED_GETFIELD=1` reconstructs the doc-era default-ON (buggy) build on the SAME binary — a clean single-variable A/B, immune to host-load confounds because both arms run back-to-back at the same load.

**A/B on `type.temporal.InstantTests` alone (the class the batch crashed on first), 8 iterations each, host load 3.5-8.6 the whole time:**

| Arm | Env | Result |
|---|---|---|
| buggy | `CRATONVM_JIT_GUARDED_GETFIELD=1` | **8/8 SIGSEGV (rc=139)** |
| fixed | (dev default) | **8/8 clean PASS (rc=0)**, `found=204` |

**13-class batch (all crashing "temporal/embeddedid/batch/sql.exec" classes in one JVM), buggy arm, 5 iterations:** 5/5 SIGSEGV (rc=139), crashing on the first class, load 3.8-5.2.

Low load throughout rules out host contention as the SIGSEGV cause (contention only ever produced the separate rc=124 HANG).

**gdb backtrace (buggy arm, `InstantTests`):**
```
Thread 2 "main-vm" received signal SIGSEGV, Segmentation fault.
#0  0x00007ffff778d12d in ?? ()          <- JIT-generated code (unsymbolicated; expected for JIT frames)
#1  0x0000000000000040 in ?? ()          <- small-int 0x40 used as a return address/pointer
#2  0x000000000000000e in ?? ()
#3  0x000002003dd74300 in ?? ()
rax = 0x4
rip = 0x7ffff778d12d
```
The `0x40` small-integer used as a pointer is an exact match to the signature `93b33576` describes for this regression ("a small-int value `0x40` [used] as an array pointer, fed by a corrupted getfield result", "segfault at 4c"). Same bug, different suite.

## Bottom line

- The 2026-07-10 Hibernate "global-temp-table SIGSEGV cluster" is the guarded-inline-getfield JIT regression, **already resolved on `dev` default builds by `93b33576`**. No new code change was needed.
- No global-temp-table / DDL-lifecycle / GC-root-pinning bug is involved. That hypothesis is refuted.
- Residual (pre-existing, separate): `DefaultCatalogAndSchemaTest` and peers can still HANG (rc=124) under host load — the documented environmental hang, not a CV-only defect.
- Latent defect: root-caused and FIXED the same day (2026-07-10), in a separate investigation --
  the WildFly Host Controller invoke-inline-cache SIGSEGV
  (`docs/internal/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver-FIXED.md`).
  The vm-side JIT field resolvers fabricated a `(0, false)` compact-field slot for any field
  with no genuine registered `CompactLayout`, and the compact-offset inline getfield arm's
  32-bit `MOVSXD` load of half a `Value` cell for a reference field is exactly this doc's
  `0x40`/`rax=0x4` "small-int used as a pointer" signature. Fixed on `dev`; guarded-inline
  getfield is re-enabled default-ON again (re-verified clean against the original ES IVF-KNN
  repro that first surfaced it -- see the follow-up on
  `docs/known-issues/elasticsearch-suite/ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md`).
  `CRATONVM_JIT_GETFIELD_HELPER=1` is now the (rarely-needed) off-switch.
