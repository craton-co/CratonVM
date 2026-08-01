# Every JIT ban is disabled — explicit inventory (2026-07-31)

All **19** ban gates in `vm/src/jit/skip_list.rs` are commented out. The gates
and their incident write-ups are left in the file; only the `return` paths are
disabled, in four places, so the whole set comes back by deleting the disabling
lines. This doc is the record of *what* was disabled and *what each one was
protecting against*, so a regression that appears after this change can be
matched to a ban without archaeology.

**Nothing here was re-verified as unnecessary.** Exactly one of the 19
(`org/h2/`) was measured this session. The other 18 are disabled on request,
not because they were shown to be stale. Treat every entry below as a live
suspect when triaging a new JIT failure.

Two `CRATONVM_JIT_BISECT_*` hooks are deliberately **still live** — they return
`SkipReason::RustJvmTestFixture` but are debugging levers, not bans, and they
are how you pin a single method without a rebuild:

```bash
CRATONVM_JIT_BISECT_SKIP='org/h2/mvstore/MVStore.commit,java/math/BigInteger.<init>'
```

`CRATONVM_JIT_BISECT_ONLY='<prefix>,<prefix>'` is the inverse (only the listed
packages stay JIT-eligible).

## Disabled, in file order

| # | Where | Target | What it protected against |
|---|---|---|---|
| 1 | `should_skip_jit_with_init` | `<clinit>`, non-trivial | A1.4 policy: `<clinit>` re-entrancy during constant-pool resolution. Had an `InitComplexity::Trivial` carve-out. |
| 2 | `should_skip_jit_with_init` | `<init>`, non-trivial | A1.4 policy: interaction with field-initialisation order. Same carve-out. |
| 3 | `should_skip_jit_internal` | `java/util/Collections.indexedBinarySearch` | ES812 postings residual: dispatches a lambda `apply` through the **wrong receiver** after a `java/util` promotion — invokeinterface PIC not invalidated on a changing lambda receiver. |
| 4 | `should_skip_jit_internal` | `java/util/stream/MatchOps.make{Int,Ref,Long,Double}` | Not correctness — *performance*. Every call reaches an internal indy that lowers to an always-deopt uncommon trap: 6928 `UnreachedCode`/`MakeNotCompilable` events for `makeInt` alone in one ES run, none of which stopped re-invocation. |
| 5 | `should_skip_jit_internal` | `<clinit>` (no init-complexity info) | As #1, on the path with no complexity classification. |
| 6 | `should_skip_jit_internal` | `<init>` (no init-complexity info) | As #2. |
| 7 | `should_skip_jit_internal` | interface default methods | A1.4: known regalloc parameter-mapping bug. |
| 8 | `should_skip_jit_internal` | any method on an **unnamed thread** | Early-init safety: thread-local JIT state may not be set up yet. |
| 9 | `should_skip_jit_internal` | `java.lang.reflect.Proxy` subclasses | **Design, not a defect.** The body is a `super.h.invoke(this, mN, args)` trampoline whose semantics CratonVM overrides *at dispatch*, not in the bytecode. Compiling the trampoline runs the wrong semantics. |
| 10 | `should_skip_jit_internal` | `java/math/MutableBigInteger` | Confirmed JIT-only array-index corruption in divide/normalisation. |
| 11 | `should_skip_jit_internal` | `java/math/BigInteger` | Whole-class; a multi-method interaction, never narrowed. **Note:** the removed SUNEC-INTPOLY ban depended on this one staying active — re-test P-384/P-521 EC keygen+sign+verify (`EcIntPolyProbe.java`). |
| 12 | `should_skip_jit_internal` | ANTLR `PredictionContext` equality/hash | Null-`PredictionContext` parse corruption. Survived the `groovyjarjarantlr4/` package lift on purpose. |
| 13 | `should_skip_jit_internal` | `org/h2/` | **The one measured entry.** See below. |
| 14 | `should_skip_jit_internal` | `org/apache/commons/logging/` | Confirmed corruption during per-class logger wiring; root cause never narrowed past "some method in the package". Was deliberately **not** liftable via `CRATONVM_JIT_ALLOW_PACKAGES` — a confirmed corruption should not have a casual opt-out. |
| 15 | `should_skip_jit_internal` | unconditional hash-miscompile cluster | Conservative-policy only. |
| 16 | `should_skip_jit_internal` | AQS family | Conservative-policy only. |
| 17 | `should_skip_jit_internal` | `KeyedReentrantReadWriteLock$LockImpl.lambda$lock$0` | One Tomcat lambda shaped `v == null ? new X() : v` — branch, allocate-and-construct on one arm, pass through on the other, merge, return. Plausibly the callee-saved-clobber family, but this exact shape (a lambda) was not covered by that fix. |
| 18 | `should_skip_jit_internal` | ConcurrentLinkedQueue family | Conservative-policy only. |
| 19 | `should_skip_jit_internal` | `org/jboss/modules/` | Tag-bit corruption in allocate-then-putfield-heavy module-graph traversal (W2-CHM / RBC.1 / SPB.1-7 archetype). |

## The one that was measured

`org/h2/` (#13) is the only entry with fresh evidence. Its own comment said
*"LIFT THIS BAN once [the TreeMap comparator bug] is fixed"*, and that blocker
is fixed:

* `TreeMapCmpProbe` — the witness for a JIT-compiled `new TreeMap<>(cmp)`
  losing its comparator — passes **40000/40000, four consecutive runs**.
* `org.h2.test.jdbc.TestMetaData`, recorded as the **sole** regression from
  lifting and caused entirely by that bug through `SelectGroups.reset()`'s
  `new TreeMap<>(session)`, passes **3/3** with the ban lifted (was FAIL 3/3).

A fresh 218-class A/B reached 112/218 per arm before being stopped, with the
picture stable throughout and **zero PASS → non-PASS regressions**:

| | banned | lifted |
|---|--:|--:|
| PASS | 88 | **89** |
| FAIL | 10 | **7** |
| HANG | 14 | 16 |

Per-class deltas: `TestCompatibility` FAIL → PASS; `TestMultiThread` and
`TestGetGeneratedKeys` FAIL → HANG (both already failing and already tracked
elsewhere — they now burn the 300 s timeout instead of failing fast, which
costs suite wall-time but is not a correctness regression).

## Restoring

Delete the disabling lines at the four sites marked `DISABLED 2026-07-31` /
`ALL JIT BANS COMMENTED OUT`. The single `return None;` in
`should_skip_jit_internal` covers entries 5–19; entries 1–2 and 3–4 are
commented out individually because they sit above the bisect hooks.

To restore just one ban, prefer `CRATONVM_JIT_BISECT_SKIP` (no rebuild) over
un-commenting, until you know which one you actually need.

## What this changes about triage

`SkipReason::RustJvmTestFixture` is now produced **only** by the two bisect
hooks. Before this change it was also the reason for entries 12, 13, 14, 17 and
19 — the variant name is historical and has nothing to do with `cratonvm/*`
test fixtures. If you see it in a JFR event or log from an older build, grep the
call sites rather than trusting the name.
