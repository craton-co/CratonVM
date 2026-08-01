# `DefaultCatalogAndSchemaTest` is intermittently unstable in every collector arm

| | |
|---|---|
| **Status** | 🔴 OPEN — pre-existing, present on `origin/dev` @ `cc8167f94` with no local changes. |
| **ID** | `HIB-DCAST-LATEPHASE.1` |
| **Found** | 2026-08-01, while re-verifying `HIB-GCOVERHEAD-HALFFULL.1` against the `dev` tip. |
| **Not** | caused by the collector fixes on `fix/hib-gcoverhead-halffull-20260731` — it reproduces with those disabled, and on `dev` without them at all. |

## The measurement

Same class, same classpath, same `--Xmx 1500m`, JIT on, real JDK. Fifteen runs:

| arm | runs | outcome |
|---|---|---|
| pure `origin/dev` @ `cc8167f94` (old gate ⇒ no selective promotion) | 5 | 4 clean; **1 ×** `ServiceConfigurationError` at inv#5, run aborted at 50 tests |
| that tree + the GC fixes, `CRATONVM_NO_SELECTIVE_PROMOTE=1` | 3 | 1 clean; **1 ×** 6 failures at 54 tests; **1 ×** 1 failure at 11 tests |
| that tree + the GC fixes, default | 4 | 2 × clean `132/132`; **1 ×** `132/132 failed=0` then SIGSEGV in teardown; **1 ×** early `capacity overflow` panic |
| base `32f9db9a2` + the GC fixes | 3 | 3 × clean `132/132` |
| HotSpot control | 2 | `132/132 failed=0`, ~120 s |

## What that shows

- **The instability is in every arm**, including collectors that never promote.
  It is not the selective-promotion drain and not the movable-JIT-root bound
  (both fixed on that branch, both argued independently).
- **Promotion-ON has the best outcomes of the three CratonVM arms**: every run
  that completed its plan reported all 132 tests passing. Both promotion-OFF
  arms produced genuine test failures and truncated runs (11 and 54 tests).
- The faces vary run to run on one binary: `ServiceConfigurationError` on
  `BytecodeProviderImpl`, truncated test counts with no error at all, an
  `AbstractMethodError` resolving to an interface's abstract declaration, a
  `capacity overflow` panic, and a SIGSEGV. That variety on fixed inputs is the
  signature of **memory corruption**, not of any one broken feature.
- One face is worth separating: a run can report `132/132 failed=0` and *then*
  SIGSEGV during teardown, before `testPlanExecutionFinished`. The test results
  in that run are correct and complete; only the shutdown crashes.

## Where to start

The crash report from the teardown SIGSEGV attributes itself:

```
#  gc young-gen actual: 5 moving cycle(s), 24 cycle(s) diverted to the NON-MOVING sweep
#  gc young-gen last incomplete-coverage reason: innermost-rbp-belongs-to-unguarded-callee
#  gc young-gen: the faulting thread had an UNREGISTERED JIT frame on its native
#     stack in the last root-gathering pass (no precise root map for it)
```

Note the first line: on this tip moving-young **does** engage (5 cycles), which
it did not on `32f9db9a2`. A tree where some young cycles copy and others sweep,
with unguarded direct JIT→JIT callees (`FOREIGN_INNERMOST_RBP`) and unregistered
frames in the mix, is a plausible place for a root to be missed — but that is a
hypothesis, not a finding, and the arm table above rules out the obvious
collector-side suspects.

Cheap next steps, in order:

1. `CRATONVM_NO_MOVING_YOUNG=1` on the `dev` tip — does the instability track
   moving-young actually engaging? One flag, one run.
2. `CRATONVM_DBG_SWEEP_LIVENESS=1` (added by dev's own `6ce9be3ab`/`e91fd7232`)
   — it asserts nothing live still points at a block the sweep frees. It did not
   fire on `probes/GcPromoteProbe.java`; it has not yet been run against this
   class for a full ~27 min.
3. Bisect `32f9db9a2..cc8167f94` — the base-plus-fixes arm was 3-for-3 clean, so
   something in dev's 187-file span is implicated. `730348352` (old-gen free
   list never coalesced), `c3dbb011a` (old-gen mark accepted unvalidated
   addresses) and `4ec8d31ad` (thread execution state machine) are the
   GC/threading-adjacent candidates.

## Cost

~27 minutes per run on a quiet box, and no arm fails reliably — the worst arm is
2 in 3, the best 1 in 5. Budget several runs per hypothesis, and prefer
`ListingRunner` over `CratonRunner`: the latter's failure dump is gated on
`failed != 0`, so it prints nothing when tests vanish at the container level.
