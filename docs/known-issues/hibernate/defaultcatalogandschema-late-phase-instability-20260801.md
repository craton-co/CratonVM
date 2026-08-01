# `DefaultCatalogAndSchemaTest` is intermittently unstable in every collector arm

| | |
|---|---|
| **Status** | 🔴 OPEN — pre-existing, present on `origin/dev` @ `cc8167f94` with no local changes. |
| **ID** | `HIB-DCAST-LATEPHASE.1` |
| **Found** | 2026-08-01, while re-verifying `HIB-GCOVERHEAD-HALFFULL.1` against the `dev` tip. |
| **Two faults, not one** | (a) *test failures / truncated runs* — pre-existing, reproduces on pure `dev` with no local changes; (b) *VM crashes* — appear only when selective promotion is enabled **on this tip**, and never on base `32f9db9a2`. See the table. |

## The measurement

Same class, same classpath, `--Xmx 1500m`, JIT on, real JDK. Sixteen runs on the
`origin/dev` tip `cc8167f94`, split by whether the non-moving sweep's SELECTIVE
PROMOTION is enabled:

| arm | runs | crashes | other |
|---|---|---|---|
| promotion **ON** (`fix/hib-gcoverhead-halffull-20260731`) | 7 | **5** (4 SIGSEGV, 1 `capacity overflow` panic) | the 2 non-crashing runs were clean `132/132` |
| promotion **ON** + `CRATONVM_NO_MOVING_YOUNG=1` | 1 | **1** SIGSEGV @118 | — |
| promotion **OFF** (`CRATONVM_NO_SELECTIVE_PROMOTE=1`, same binary) | 3 | **0** | 1 clean; 1 × 6 failures @54; 1 × 1 failure @11 |
| promotion **OFF** — pure `origin/dev`, no local changes | 5 | **0** | 4 clean; 1 × 1 failure @50 |
| promotion **ON**, on base `32f9db9a2` instead of the dev tip | 3 | **0** | 3 × clean `132/132` |
| promotion **ON** + `CRATONVM_NO_OLDGEN_COALESCE=1` | 2 | **0** | 1 clean `132/132`; 1 × 8 failures and progress collapsing to ~2 tests/20 min |
| HotSpot control | 2 | 0 | `132/132`, ~120 s |

**6 crashes in 8 promotion-ON runs; 0 in 8 promotion-OFF runs.** And promotion-ON
on the *older base* was 3-for-3 clean. So:

- Selective promotion is **unsafe on the `dev` tip and was not on `32f9db9a2`**.
  Something in dev's 187-file span between those two commits either broke it or
  exposed a latent unsafety in it.
- It is not moving-young engaging: the crash survives `CRATONVM_NO_MOVING_YOUNG=1`.
- It is not the movable-JIT-root exclusion: that bound is already in the
  promotion-ON binary (see
  [`invocation12-...-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/invocation12-late-phase-instability-movable-jit-root-20260801-FIXED.md)),
  and the crashes persist through it.
- Separately, and independently of promotion, the class fails *tests* on this tip
  in every arm (11, 50, 54-test truncations with real failures). That older
  instability is what this document was originally filed for.

⚠️ **This blocks merging `fix/hib-gcoverhead-halffull-20260731` into `dev`.** The
GC-overhead fix on that branch is correct and independently proven (see its own
doc, and `probes/GcPromoteProbe.java`, which still wedges on the `dev` tip) — but
it cannot land until this is understood, because landing it turns a heap wedge
into a VM crash.

### All three crashes attribute themselves identically

```
#  gc young-gen actual: 3-5 moving cycle(s), 24-28 cycle(s) diverted to the NON-MOVING sweep
#  gc young-gen last incomplete-coverage reason: innermost-rbp-belongs-to-unguarded-callee
#  gc young-gen: the faulting thread had an UNREGISTERED JIT frame on its native
#     stack in the last root-gathering pass (no precise root map for it)
```

## Where to start

Note the moving-cycle count above: on this tip moving-young **does** engage,
which it did not on `32f9db9a2`. That is not the cause on its own — the crash
survives `CRATONVM_NO_MOVING_YOUNG=1` — but it does mean the tip mixes copying
and sweeping young cycles in one run, alongside unguarded direct JIT→JIT callees
(`FOREIGN_INNERMOST_RBP`) and unregistered frames.

Cheap next steps, in order — the top one is a specific, testable hypothesis:

1. **`730348352`'s old-gen coalescer is the leading suspect, and it was tested.**
   That commit ships its own escape hatch, so no rebuild is needed:
   `CRATONVM_NO_OLDGEN_COALESCE=1`. With it set, **2 runs produced 0 crashes**
   where the same binary crashed 6 times in 8 without it. Selective promotion is
   the only thing on either branch that allocates into old gen at volume, and a
   coalescer that widens a free block over a live promoted object corrupts
   exactly like this.

   That is suggestive, **not conclusive** — n=2, and the flag is not a viable
   configuration in its own right: the second run degenerated to ~2 tests per
   20 minutes, which is precisely the un-coalesced free-list collapse
   `730348352` exists to fix. So the answer is not "turn coalescing off"; it is
   to find why coalescing and freshly-promoted old-gen objects disagree. Start
   at `OldGen::coalesce_free_blocks` and ask what can put a block on the free
   list while a promoted object still occupies it — the selective-promotion
   evacuate pass installs `GC_FLAG_OLD_GEN` and bumps `bytes_promoted` at
   `gen_heap.rs` ~6800, and its "(3) fix up references … then dirty cards" pass
   runs later, so there is a window.

   Sibling candidate, untested: `c3dbb011a` ("the old-gen mark must not accept
   unvalidated addresses, and old-gen liveness must be free-list aware").
2. `CRATONVM_DBG_SWEEP_LIVENESS=1` (dev's own `6ce9be3ab`/`e91fd7232`) for a full
   class run — it asserts nothing live still points at a block the sweep frees.
   It does **not** fire on `probes/GcPromoteProbe.java`, which promotes megabytes
   per cycle, so whatever this is needs the class's shape.
3. Bisect `32f9db9a2..cc8167f94` with promotion forced ON, 3 runs per point.

## Cost

~27 minutes per run on a quiet box, and no arm fails reliably — the worst arm is
2 in 3, the best 1 in 5. Budget several runs per hypothesis, and prefer
`ListingRunner` over `CratonRunner`: the latter's failure dump is gated on
`failed != 0`, so it prints nothing when tests vanish at the container level.
