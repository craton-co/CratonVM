# Proposal: stop handing the collector the initiator's roots three times

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 39
> of 54).** Step 10b landed (w6-a); steps 11 and the GC-entry JIT rescan not
> built. **Gate:** `CRATONVM_DBG_ROOTPROF=1` on `MtChurnProbe` /
> `ConcurrencyUnderGcSweep`, and the snapshot-vs-direct diff empty on the
> battery. **Size:** S.

> **STATUS (2026-09-26): partly implemented; the larger duplicates stay
> OPEN.** The first step landed in gc-common w6-a (`8e8e1cac8`):
> `run_collection_pause` calls `memory::roots::collect_roots_registry_appended`
> (`vm/src/memory/roots.rs`, line 1123), which skips step 10b because the
> caller's `collect_all_root_snapshots()` append pushes the same thread
> mirrors, so each mirror now reaches the marker once. Tests:
> `roots::tests::the_registry_mirror_skip_is_scoped_and_restored` and the
> source ratchet `the_registry_appended_scan_is_always_followed_by_the_registry_append`.
> Not landed: skipping step 11 (the initiator's `root_snapshot`, which its
> registry entry carries again) needs a registry accessor such as
> `ThreadRegistry::shares_root_snapshot(tid, &Arc)` (`Arc::ptr_eq` plus
> `alive`), which does not exist; dropping the initiator's second
> conservative JIT scan (`update_root_snapshot` at GC entry, then
> `collect_roots` step 14) needs the snapshot-vs-direct-scan diff from the
> Risk section, measured with `CRATONVM_DBG_ROOTPROF=1` on `MtChurnProbe` and
> `ConcurrencyUnderGcSweep`. Since wave 2 the snapshot also carries the JIT
> deopt stash, which step 10 pushes too (two thread-local probes when the
> stash is empty).

**Status:** OPEN (proposal) — filed 2026-09-23 by gc-common round, wave 1, lane B.

## Evidence

On the multi-thread STW path (`maybe_gc`, `vm/src/runtime/interpreter/gc_and_alloc.rs`)
the initiator:

1. runs `update_root_snapshot(shared, thread)` — its own frames, JIT frames
   (a full conservative `scan_active_jit_frames`), shadow stack and off-frame
   roots, into `thread.root_snapshot`;
2. calls `collect_roots`, which scans the same frames again (section 1), the
   same JIT frames again (section 14), and then pushes `thread.root_snapshot`
   wholesale (section 11);
3. appends `thread_registry.collect_all_root_snapshots()`, which includes the
   initiator's own (alive) registry entry — the same `root_snapshot` Arc — a
   third time, plus every alive thread's `java_thread_obj`, which
   `collect_roots` section 10b (`alive_thread_objects`) already pushed.

So every initiator frame root reaches the marker three times and each thread
mirror twice, and the initiator's native stack is conservatively scanned twice
per pause. Duplicates are cheap for a marker with a mark bit, but not free:
G1's root-to-CSet pass, ZGC's root filter and the pin-set construction all
iterate the vector, and the conservative scan is the expensive part
(`[rootprof] scan_active_jit_frames by caller`, `CRATONVM_DBG_ROOTPROF=1`).

## Proposal

* Skip section 11 when the caller is about to append
  `collect_all_root_snapshots` (pass a flag, or have `collect_all_root_snapshots`
  take the initiator's `ThreadId` and skip its entry — the initiator is covered
  by `collect_roots` directly).
* Do not re-run the JIT frame scan in `update_root_snapshot` for the
  initiator at GC entry: the snapshot it builds is only read back by the
  initiator's own `collect_roots`.
* Measure first: `CRATONVM_DBG_ROOTPROF=1` (`collect_roots took`), root-vector
  length before/after on `MtChurnProbe` and `ConcurrencyUnderGcSweep`.

## Risk

The snapshot and the direct scan use slightly different filters
(`scan_frame_roots` vs section 1). Before dropping either, diff the two sets on
the probe battery; keep the union if they differ.
