# GC_STRESS — residual corruption faces (post concurrent-old-gen fix)

**Status:** 🟡 OPEN. Split off `gcstress-concurrent-oldgen-races-FIXED.md`
(moved to `docs/internal/` — the three concurrent-old-gen defects it
investigated are fixed on dev, commit `57f545be`). These are **different**
corruption signatures that survive that fix under the aggressive
`CRATONVM_DBG_GC_STRESS` lane. Not yet root-caused; not yet confirmed
distinct from each other.

## Repro

```
CRATONVM_REAL_FORKJOINPOOL=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
CRATONVM_DBG_GC_STRESS=65536 \
  <cratonvm>.exe --java-home "C:/Program Files/Java/jdk-25" \
  -cp docs/known-issues/repros/A4-fork6 Fork6Hard 128 20   # also --nojit
```

Load-nondeterministic: per the fork6-fjp doc's own history, distinguishing a
partial fix from noise on this lane needs ≳100 interleaved runs. A single
run failing is not by itself evidence against a fix.

## Residual faces

1. **Stale bootstrap-era raw pointer in Value cells.** Recurring
   `gen_heap::read_slot: corrupt Value cell` with `raw0=0x1463f0088` (or
   `0x1463f00f0`) and `raw1=0` — the same one-or-two bootstrap-arena
   addresses appearing as raw 8-byte pointers inside 16-byte Value cells at
   young (`0x80…`) and old (`0x102…`) holders, long after the young arena
   has grown away from `0x1463f…`. Suspects: young-arena grow (realloc)
   remap gap for some holder class, or an 8-vs-16-byte slot-layout confusion
   writing a compact ref into a legacy Value cell.
2. **JIT lost-tag int-in-ref-slot.** `JIT dispatch into ForkJoinTask.doExec
   failed: expected object reference, got int(512)` + walker headers like
   `kind=Object array_length=512 num_slots=4 class_id=6` — an A4-family
   tag/layout confusion under JIT, distinct from GC reclamation. May be
   related to the still-open fork6-fjp A4 register-only residual (see
   [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md)) —
   unconfirmed.
3. **Bootstrap `set_field` OOB write-drop** at `obj=0x1023f1328` (a 0-field
   `ClassId(0)` ad-hoc container written at index 0) — present as the FIRST
   log line of every stress run, pre- and post-fix, JIT and `--nojit`.
   Possibly a benign pre-existing bootstrap quirk, but a silently dropped
   write is a latent-null source and should be identified before being
   dismissed.

## Diagnostics available

`CRATONVM_DBG_MTROOTS=1` (per-GC initiator dump + blocked census),
`CRATONVM_GC_ARRAY_GUARD_BT=1`, `CRATONVM_DBG_SWEEP_ZERO=1` (returned no hits
on this lane — that detector covers the young non-moving sweep, not the
old-gen concurrent sweep, so it would not have caught the now-fixed defects
either; confirm before trusting a "clean" `SWEEP_ZERO` run on any old-gen
corruption).

## Next steps

1. Reproduce each face in isolation with a targeted, deterministic repro
   (parallel to `HwBlocked.java` in `repros/A4-fork6/`) rather than the noisy
   aggressive stress lane.
2. For face 1: check the young-arena `grow()`/realloc path for a stale
   pre-grow address baked into any long-lived structure (thread-local cache,
   GC diagnostic sample, etc.) that survives past the grow.
3. For face 2: determine whether this is the SAME register-only oop gap as
   A4, or a distinct tag-tracking bug in the compact-ref-fields layout under
   `CRATONVM_REAL_FORKJOINPOOL`.
