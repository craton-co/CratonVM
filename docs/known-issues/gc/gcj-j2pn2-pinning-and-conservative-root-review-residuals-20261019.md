# Pinning, conservative roots, pinned young copy: review residuals of gcj j2/pn2

> **STATUS (2026-10-19, gcj j3/rt3): item 1 (`thread_id_for_os_tid_inner`'s stale claimant) FIXED IN CODE, PENDING BUILD (commit `fdcbbfa392`).** The identity lookup admits a claim only through `os_tid_claim_admitted(os_tid, mounted, skip)` -- the capture fold's rule -- so an unmounted continuation never answers; latched per registry as `CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED` (default ON, `=0` kills), every backend (thread identity: an unbound JNI `MonitorEnter` was owned under, and an unbound caller's pause identity named, the parked continuation). Test `threading::thread_registry::tests::gcj3rt3_a_carrier_after_an_unmount_is_answered_with_itself`. The other items are unchanged.

> **STATUS (2026-10-19, gcj j2/pn2): OPEN -- found by reading; nothing here changed in code.** Line numbers: branch `gcj2-pn2`.

1. **The carrier's own identity query has the same stale-claimant rule.**
   `vm/src/threading/thread_registry.rs` `thread_id_for_os_tid_inner` (~7120, behind
   `thread_id_for_current_os_tid`) answers the newest `set_os_tid_current` on the OS thread. A
   virtual thread's mount publishes; its unmount does not republish the carrier. So a carrier that
   asks "who am I" from its own OS thread after a continuation parked is answered with the PARKED
   continuation (the doc's "a mount publishes before it runs any Java, so the running identity's
   publish is the last one made here" holds for a mount, not for the carrier after an unmount).
   Callers include the STW initiator census (`alive_count_blocked_and_os_tids_for_inner`). Fix: the
   `mounted` screen this lane added to `newest_claimants_by_os_tid` (`os_tid_claim_admitted`), or a
   republish of the carrier's claim at unmount. Owner: common / threading (cm); needs a row with
   virtual threads plus `CRATONVM_DBG_STW_EXPECTED_IDS`.
2. **Parallel refusals past the shard cap.** `gc/src/gen_evac.rs` `REFUSALS_PER_SHARD` (64): a pinned
   object refused past the cap is not seen by `absorb_parallel_in_place`'s
   `note_refused_forward` (gcj j2/pn2) and is still dropped by the drop mode. Fix: a separate
   uncapped per-shard list of refused addresses that are pinned bases (`ParInPlace::is_pinned_base`).
3. **`YoungPinRange` cannot be narrowed through `JIT_REGION_BOUNDS`** (j1 ledger item 2): the table is
   the JIT's inline young-containment table too. A second published pair (from-space base, its
   allocation top at the pause) is needed; not built.
4. **`note_write` classifies against its own SP** (j1 raw-write item 3): needs the guard's band as an
   argument at the five writers.
5. **Process: loop rows were not extracted.** `vj1-pn.list` holds only the 14 unrolled rows of the
   j1-pn report; every `for r in ...` block (PK def/kill, PX, PL off/on, H4JM def/se0) has no j1a
   reading, so e.g. "PL" is VOID on j1a and h4jm's def arm was never run. This report writes every row
   unrolled.
6. **The census arm changes the run.** The h4jm census row's failing pair (private, spliced) differs
   from every earlier non-census reading (virtual / interface / spliced), as w9c's census arm did
   (`FAIL 3 of 6`). Judge the def / flip arms on their own stdout, and use the census only to name a
   holder.
