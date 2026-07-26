# GC_STRESS — concurrent old-gen collection races (Fork6Hard lane) — FIXED

**Status:** ✅ **FIXED on dev** (merged from `fix/oldgen-concurrent-mark-races-20260703`,
commit `57f545be`). Three unambiguous live-object-freeing defects in the
concurrent old-gen mark/sweep, described below, are fixed and unit-tested.
Moved to `..` per the known-issues triage rule: the *primary*
defects this doc investigated are resolved. A later **residual** with different
corruption signatures was fixed separately and is archived at
[gcstress-residual-corruption-faces-FIXED.md](gcstress-residual-corruption-faces-FIXED.md).
Do not re-investigate the three defects below; they are closed.

Split out of
[fork6-fjp-multithread-jit-root-reclamation-FIXED.md](fork6-fjp-multithread-jit-root-reclamation-FIXED.md)
(2026-07-02 section), which established this family is JIT-free — it
reproduces under `--nojit` and with `any_thread_in_jit=false` at every STW.
The GC_STRESS lane failures previously attributed to fork6-fjp's A4 (register-
only JIT root residual) were NOT A4 — that mis-scoping is corrected in the A4
doc and in `../../known-issues/README.md`.

## Repro

```
CRATONVM_REAL_FORKJOINPOOL=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
CRATONVM_DBG_GC_STRESS=65536 \
  <cvmp>.exe --java-home "C:/Program Files/Java/jdk-25" \
  -cp <repros/A4-fork6 classes> Fork6Hard 128 20      # also --nojit
```

Faces pre-fix: deterministic rep-0 `ExecutionException: NullPointerException`,
`gen_heap::get_field/set_field` OOB on all-zero (`class_id=0 num_slots=0`)
receivers, and under `--nojit` an instant `<clinit>`-phase death
(`Exception in thread "main" java/lang/Object` from `ForkJoinPool.<clinit>` —
a corrupted throwable). Or a CPU-pegged wedge: under stress
`needs_gc()` compares the young high-water cursor (never retreated by the
non-moving sweep) against the threshold, so after the first 64 KiB it is
permanently true and every `maybe_gc` collects — the "wedge" is a quadratic
GC storm by design of the stress hook, not a hang.

## The three fixed defects (all in the `maybe_concurrent_gc` cycle)

1. **The SATB write barrier was never wired.** `enable_concurrent_gc` had no
   production caller: the heap's `concurrent_gc_state` stayed `None`, so
   `satb_barrier` was a hard no-op in every run — and each cycle's
   `ConcurrentMarker::new` built a private queue/state that no mutator could
   reach anyway. The "concurrent" mark ran against live mutators with no
   write barrier; any sole reference moved during the mark phase lost its
   target to the sweep. Fixed: `SharedVm` construction attaches a shared
   `SatbQueue` + `ConcurrentGcState` to the heap (`concurrent_satb` /
   `concurrent_gc_state` fields) and every cycle's marker is built on the
   same handles (`ConcurrentMarker::with_shared`). `deposit_root_snapshot`
   now also flushes the thread-local SATB buffer so a thread parking
   mid-cycle doesn't sit on undrained entries.

2. **Young→old references were never traced.** `initial_mark`/`remark`
   filter the root list with `old_gen.contains`, and nothing walked young
   objects' fields — an old object whose only path is
   root → young holder → old target was invisible and swept live. Selective
   promotion (default-on) mass-produces exactly that shape by tenuring a
   pinned young holder's children, so old cycles under allocation pressure
   freed live promoted objects — even single-threaded (the `<clinit>` face:
   `alive=1` at the corrupting STW in the MTROOTS census). Fixed:
   `GenerationalHeap::collect_young_to_old_roots()` (hardened young walk +
   `for_each_ref_slot`, old-filtered) feeds both STW root sets.

3. **A failed remark STW fell through to the sweep.** Phase 3's
   `brief_stw_counted` returns `false` when another STW wins the race — a
   near-certainty under the stress storm, where a young-GC request is always
   pending — and the return value was ignored: the sweep then ran with a
   NON-final bitmap (SATB undrained, roots unrescanned). Phase 1 checked its
   result; phase 3 did not. Fixed: `!remark_done` → `marker.abort_cycle()`
   (deactivate barrier, discard bitmap, phase Idle) and skip the sweep.

Focused tests (`cargo test -p cratonvm-gc --lib`, 42/42 green):
`with_shared_marker_marks_satb_entries_from_shared_queue`,
`abort_cycle_deactivates_barrier_and_resets_phase`,
`collect_young_to_old_roots_finds_young_held_old_target`.

## Validation

- Controls on the fixed binary: `Fork6` ALL-OK, `Fork6Hard 256 40` ALL-OK,
  bt16 checksum golden (`14985902`, wall in line with same-load baseline).
- `--nojit` stress lane: the instant `<clinit>` death is GONE (pre-fix it
  died before user code; post-fix the rep loop runs minutes before residual
  faces appear).
- JIT-on stress lane (args-dropped harness variant = `N=256 reps=400`,
  HARDER than canonical): pre-fix rep-0; post-fix reached `rep=30` before a
  *different* face. The canonical `128 20` lane remains load-nondeterministic
  (a post-fix run still failed at rep 0 under heavy box load) — per the A4
  doc's history this lane needs ≳100 interleaved runs to separate a partial
  fix from noise; the three defects above are proven at the code level and
  unit-tested rather than lane-validated.

## Residual faces (OPEN — different mechanisms, this doc tracks them)

1. **Stale bootstrap-era raw pointer in Value cells.** Recurring
   `gen_heap::read_slot: corrupt Value cell` with `raw0=0x1463f0088` (or
   `0x1463f00f0`) and `raw1=0` — the same one-or-two bootstrap-arena
   addresses appearing as raw 8-byte pointers inside 16-byte Value cells at
   young (`0x80…`) and old (`0x102…`) holders, long after the young arena
   grew away from `0x1463f…`. Suspects: young-arena grow (realloc) remap gap
   for some holder class, or an 8-vs-16-byte slot-layout confusion writing a
   compact ref into a legacy Value cell. Also seen pre-fix; unaffected by
   the old-gen fixes.
2. **JIT lost-tag int-in-ref-slot.** `JIT dispatch into ForkJoinTask.doExec
   failed: expected object reference, got int(512)` + walker headers like
   `kind=Object array_length=512 num_slots=4 class_id=6` — the A4-family
   tag/layout confusion under JIT, distinct from GC reclamation.
3. **Bootstrap `set_field` OOB write-drop** at `obj=0x1023f1328` (a 0-field
   `ClassId(0)` ad-hoc container written at index 0) — present as the FIRST
   log line of every stress run including pre-fix and `--nojit`; possibly a
   benign-ish pre-existing bootstrap quirk, but a silently dropped write is
   a latent-null source and should be identified.

Diagnostics used: `CRATONVM_DBG_MTROOTS=1` (per-GC initiator dump +
blocked census), `CRATONVM_GC_ARRAY_GUARD_BT=1`, `CRATONVM_DBG_SWEEP_ZERO=1`
(no hits — the sweep-zero detector covers the young non-moving sweep, not
the old-gen concurrent sweep, which is why the old-cycle frees were
invisible to it).
