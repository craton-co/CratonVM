# Raw root write-backs and their audit: owner and classification findings (gcj j1/pn)

> **STATUS (2026-10-19, gcj j2/pn2): ITEMS 1, 4, 5, 6 FIXED IN CODE, PENDING BUILD/PROBE; items 2, 3, 7 open.** Item 1: `vm/src/threading/thread_registry.rs` `newest_claimants_by_os_tid` takes `skip_unmounted` (`os_tid_claim_admitted`): an unmounted entry claims no OS tid, so a blocked carrier is again the claimant of its own stack's captures. `CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED` (default ON, `=0` kills), applied only when the fold's heap `is_generational()` (G1 / ZGC folds unchanged). Test `gcj2pn2_an_unmounted_continuation_claims_no_carrier_tid`; probe `tools/probes/Gcj2Pn2VtCarrierChurnProbe.java` (rows VT in `j2-pn2-report.md`; positive control: `CRATONVM_DBG_ROOT_WRITE_AUDIT=1` on the `=0` arm prints the `fold declined ... it claims that OS thread but is not the claimant running there` line, which the default arm should print less often or never for a carrier). Item 4: the skipped write-back in `vm_exec.rs` `apply_native_slot_fixups` calls `note_write(.., false)`. Item 5: `note_band_overrun`, `note_foreign_capture` and `thread_registry.rs` `note_declined_stale_claim` print nothing in ring mode (counts kept). Item 6: Windows `raw_stderr_write` uses `GetStdHandle(STD_ERROR_HANDLE)` + `WriteFile` (no Rust lock, no allocation); other non-Unix targets keep `std::io::stderr`. Item 3 needs `note_write` to take the guard's band (a signature change across five writers): open.**

> **STATUS (2026-10-19, gcj j1/pn): OPEN -- found by reading; nothing changed in code.** Items 1-2 are the cases `docs/internal/proposals/gc/gengc-r5w1-crash5-proposal-owner-checked-raw-root-writes-20260926.md` exists for; items 3-6 are `gc/src/root_write_audit.rs` defects. Generational (blocked-peer captures and their fold are the Generational moving path's).

1. **Soundness, verify on a row: an unmounted continuation stays the newest claimant of its
   carrier's tid.** `vm/src/threading/thread_registry.rs` `newest_claimants_by_os_tid` (~906)
   builds claims from every ALIVE entry with a non-zero `os_tid`; it does not look at `mounted`. A
   mount publishes a fresh `os_tid_seq` for the virtual thread (`vm/src/vm/jdk_continuation.rs`,
   the mount path), and nothing republishes the carrier's own entry at unmount. After the virtual
   thread parks it remains the newest claimant; the fold then refuses it for being unmounted and
   refuses the carrier for not being newest, so a blocked carrier's native-stack captures are
   never adopted and its words keep from-space addresses after a Cheney move. Fix: skip
   `!e.mounted` entries of a virtual thread in `newest_claimants_by_os_tid`, or republish the
   carrier's tid at unmount. Row: a virtual-thread workload whose carrier blocks in a native while
   young cycles move (`CRATONVM_DBG_ROOT_WRITE_AUDIT=1`, count `declined-stale-claim` lines).
2. **Residual: the write-back bound admits the wake path's own frames.**
   `vm/src/vm/vm_exec.rs` `apply_native_slot_fixups` (~8530) takes `SlotBand::current_thread()`
   at the write, so every frame between the blocked-region entry and the write
   (`check_post_block_gc_refs`, the fixup itself) is in band, though none of them existed at
   capture. Only the `p.read() == orig` value check protects such a word. Fix: also require
   `ns.addr >= ` the SP recorded at blocked-region entry (beside `blocked_wait_lo`).
3. **Wrong place in the audit.** `root_write_audit.rs` `classify_slot_with_sp` (~215) classifies
   against its own, deeper SP, while the guards' bands start at `scanner_sp` or the fixup's band.
   A store refused below the guard's lower bound but above the probe logs `place=own-live
   written=false`, which contradicts itself. Fix: `note_write` takes the guard's band.
4. **A skipped native write-back is not audited**, contrary to the module doc ("a word that no
   longer reads its captured value logs `written=false`"): the skip branch in
   `apply_native_slot_fixups` only bumps a counter. Fix: `note_write(.., false)` there.
5. **Ring mode still prints on the capture side.** `note_band_overrun`, `note_foreign_capture`
   (`root_write_audit.rs` ~308-336) and `thread_registry.rs` `note_declined_stale_claim` call
   `eprintln!` whatever `ring_mode()` says; ring mode exists to stop printing (prints change the
   timing a crash needs). Fix: route them into the anomaly ring.
6. **The Windows crash dump takes the stderr lock.** `raw_stderr_write` (~977) uses
   `std::io::stderr().write_all` off Unix, which locks; a thread that crashed inside `eprintln!`
   deadlocks its own dump. Fix: `WriteFile(GetStdHandle(STD_ERROR_HANDLE))`.
7. Cosmetic: the ring is single-writer by contract (two writers `RING_LEN` apart can tear a record
   that passes the seq check); budgets, rings and counters are process-global and records carry no
   VM id, so co-resident VMs mix records. No unit test covers `note_write`'s budget cut-over, the
   refusal classification (item 3), or the capture-side functions.
