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

> **2026-07-03 investigation round (branch `fix/gcstress-residual-diag-20260703`,
> binaries `cvmp-residual-diag{,2,3}-20260703.exe` with symbols
> [`strip="none"`+`debug="line-tables-only"`, local Cargo.toml tweak, not
> committed]):** new `CRATONVM_DBG_CELLCORRUPT` holder-identification
> diagnostics landed, TWO more real bugs fixed, face 3 root-caused and
> CLOSED, and face 1 narrowed to a specific writer hunt. Details below.

1. **Stale bootstrap-era raw pointer in Value cells — NARROWED, writer still
   unidentified.** `CRATONVM_DBG_CELLCORRUPT` identifies the holders: real,
   live objects (`Fork6Hard$StrTask` num_slots=6 at indexes 0/2/4/5;
   `ForkJoinWorkerThread$InnocuousForkJoinWorkerThread` Thread mirror
   num_slots=22 at index 19 ≈ `Thread.holder`) whose 16-byte Value cells
   contain `{raw 8-byte pointer, 0}` where the pointer targets a REAL early
   bootstrap object (a `java/lang/String`, plain Objects) — usually in the
   FLIPPED young semispace (`young_to`), i.e. a pre-Cheney address. The
   tagged reader rejects the untagged cell → returns null → the
   `nullchild` / "Cannot read field threadStatus because holder is null" /
   NPE faces; remap walkers can't decode the cell either, so it stays stale
   forever. Corruption happens while the holder is YOUNG; promotion then
   copies the corrupt cell verbatim to old gen.
   **Excluded writers:** `set_array_element` on a stale array ref (a
   CELLCORRUPT-gated non-array trap was live in the run that produced a
   corrupt cell and did not fire); the `ensure_system_stdin_object` stale
   write (fixed, see below — removed SOME producers: post-fix runs show
   fewer/none in some windows, but the face still reproduces).
   **Remaining suspects:** raw 8-byte writers — `write_prim_element`
   callers outside set_array_element, `Unsafe.putLong/putReference*` byte-
   vs-slot offset translation, arraycopy fast paths. Next: write-side trap
   in `write_prim_element` (Reference writes with holder-header check) or a
   memory watchpoint on a corrupt-cell address (they are deterministic).
   **2026-07-03c:** a `Fork6Hard StrTask`-triggered occurrence of this face
   was observed to fully HANG the process (ForkJoinPool workers parked
   forever, ~45s total CPU burned over 3+ hours wall-clock — not a spin
   loop), not just surface as NPE/nullchild as previously documented. The
   worker that hit the stale-pointer receiver fell back to a placeholder
   `java/lang/Throwable` (interpreter's stale-pointer recovery path) and
   the pool then appears to wait forever for that task's real completion
   signal, which the fallback never produces. Repro binary
   `cvmp-mapfix2-20260703.exe` (unrelated fix on board — see below), lane
   `Fork6Hard 128 3 --nojit GC_STRESS=65536`, hang began ~11s into the run
   and was killed after 3h+ idle. Not yet confirmed whether non-stress or
   shorter Fork6Hard runs also hang on this face or only fail fast; add to
   the fork6-fjp A4 investigation as a new symptom class.
2. **JIT lost-tag int-in-ref-slot.** `JIT dispatch into ForkJoinTask.doExec
   failed: expected object reference, got int(512)` + walker headers like
   `kind=Object array_length=512 num_slots=4 class_id=6` — an A4-family
   tag/layout confusion under JIT, distinct from GC reclamation. May be
   related to the still-open fork6-fjp A4 register-only residual (see
   [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md)) —
   unconfirmed. Untouched this round (all diagnosis ran `--nojit`).
3. **Bootstrap `set_field` OOB write-drop — ROOT-CAUSED AND FIXED
   (2026-07-03).** The `CRATONVM_DBG_OOBFIELD=Object` backtrace named
   `ensure_system_stdin_object` (vm_util.rs): the freshly allocated
   System.in FileInputStream was held in a Rust local across the
   FileDescriptor class-load/`<clinit>`/alloc window (the A1 "Rust local
   across allocating calls" family); a stress-triggered MOVING young GC in
   the window relocated it and the subsequent `set_field` wrote through the
   stale pre-move address — then cached the stale ref in
   `shared.system_in`, which was ALSO missing from both the root scan and
   the GC remap (out/err had both), poisoning every later use. Fixed
   (pin + re-read via `native_pin_roots`; `system_in` rooted + remapped;
   plus a latent RwLock self-deadlock found & fixed in the process: the GC
   paths now `try_read`/`try_write` the stream caches because the
   initializers hold the write guard across allocating calls — the naive
   `read()` wedged instantly, 0-output). Post-fix, the bootstrap write-drop
   is gone from all runs (0 hits vs deterministic-first-line before).

4. **Map/HashMap Node alloc-then-stale-local family — FIXED, unrelated to
   face 1.** Separately from this investigation, an audit of native map
   mutators found the classic A1 "bare `ObjectRef`/`Value` local held
   across an allocating call" bug in `native_map_put`, `map_resize`,
   `native_lhm_put`, `lhm_alloc_node`, `native_tm_put`, and
   `map_alloc_node` (`native-collections/src/lib.rs`) — each wrote a
   key/value/bucket-array local into a freshly `alloc_object`'d node
   *after* the allocation, without pinning, so a moving young GC during
   the alloc could relocate the arg and the write would store a stale
   (dangling) reference. Fixed by pinning + re-reading via
   `native_pin_roots` across each allocation (six commits on
   `fix/gcstress-residual-diag-20260703`). Validated on the aggressive
   GC_STRESS lane: went from dozens of corrupt-Value-cell reads + many
   CELLCORRUPT holder dumps + nullchild/NPE THREWs (pre-fix) to zero
   escalating corruption (0 corrupt-cell reads, 0 holder dumps) across two
   full runs post-fix. One PRE-COPY-only flag persisted **byte-identically**
   (same `src_obj`/`cell`/`raw` bytes) in both the pre- and post-
   `map_alloc_node` runs — since it never escalates to an actual corrupt
   read and its class (`class_id=414`, `num_slots=7`) doesn't match the
   map `$Node` layout (`NODE_NUM_FIELDS=4`), it appears to be an unrelated,
   likely-benign, deterministic bootstrap-time guard trip — not pursued
   further here.

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
