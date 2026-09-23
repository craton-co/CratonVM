# Round 10 `gcs2` — proposed directions

Lane `gcs2`'s brief was GC safety, safepoints and code-cache lifetime across
`jit/src/implicit_null.rs`, `jit/src/x64/safepoint.rs`, `jit/src/exec_memory.rs`,
`vm/src/jit/conservative_roots.rs`, `vm/src/jit/xt_root_scan.rs`,
`vm/src/jit/code_cache_lifecycle.rs` and `vm/src/jit/alloc_class_cache.rs`. No
new crash-class defect was found in this pass beyond the two the prior `gcs2`
lane already fixed in `exec_memory.rs` (the `checked_add` overflow in
`read_i32`'s bound and the drop-time `implicit_null::unregister_range` call) —
this file records directions worth someone's time rather than gaps this lane
could act on directly.

## 1. Finish the implicit-null retirement diagnostics that already exist

Two `pub fn`s in `jit/src/implicit_null.rs` — `stale_duplicates_retired()` and
`unlinked_chain_nodes()` — are read by nothing in the workspace (see
`docs/known-issues/jit/r10-gcs2-implicit-null-diagnostic-counters-orphaned-20260921.md`).
Both exist specifically so a regression in the two properties this module's
header spends the most words on (hazard 2's self-repair, and the 2026-09-16
chain-splice fix) would be *visible* rather than silently reintroduced. The
fix is small — two more reads next to the existing
`cratonvm_jit::implicit_null::counts()` call in `vm-cli/src/main.rs` — and
belongs to whoever owns that file. Worth doing in the same pass:
`vm/src/jit/code_cache_lifecycle.rs`'s `record_allocation_failure` /
`record_capacity_bytes` / `record_free_space` free functions have the same
shape of gap (see the sibling known-issue doc) — nothing in
`jit/src/lib.rs`/`jit/src/platform.rs` calls them, so `failed_allocations` and
`capacity_bytes` read zero on every real run. Both gaps are instances of the
same failure mode this round's `obs` lane independently found and fixed once
already in this same file's history (§5 of the module header): a counter with
a slot and a doc but no producer reads as "never happens" forever.

**Proposal:** when either of those files is next touched by a lane that owns
it, wire the four orphaned functions in in one pass, and add a workspace-level
ratchet test (in a location a future round can own) that greps for every
`pub fn` in these two files and fails if a name in an explicit "known
diagnostic API" allowlist has zero call sites outside its own file and its own
test module. That would have caught the original drop-hook bug this round
fixed, both of this pass's findings, and the `osr_compile_declined` /
`osr_optimizing_artifact_reused` pair the `obs` lane found the same day —
four independent instances of the identical defect shape across three lanes in
one round is a pattern, not a coincidence, and a mechanical check is cheaper
than another four rounds of manual enumeration.

## 2. Per-registration retirement instead of per-page chains

`jit/src/implicit_null.rs` already carries this proposal in its own trailing
`REVIEW-NOTE` (item 2, unassigned): `register` could return the slot index it
claimed, `CompiledMethod` could keep that `Vec<u32>`, and `drop` could retire
exactly those slots — O(sites in this method), no chain walk, no dead nodes
accumulating on a hot page's bucket, no `MAX_RANGE_PAGES` sweep fallback. This
lane re-derived the same conclusion independently while reviewing the
retirement path and did not find anything wrong with the existing per-page
design (the 2026-09-16 splice fix already closed the one real defect in it),
so this is a re-affirmation rather than a new finding: the per-page chains are
correct today, and the per-registration design is the next efficiency step
whenever `CompiledMethod::drop` in retirement actually shows up in a profile.
It needs a new `register_at` returning `Option<u32>`, a field on
`CompiledMethod` (`jit/src/lib.rs`), and a driver-loop change
(`jit/src/x64/driver.rs`) — three files this lane does not own, which is the
same reason the original note gives for not building it here.

## 3. A `QuiescenceSource` abstraction for `code_cache_lifecycle.rs`'s model — reconsidered, still not recommended

The module's own header records that `NOTES-runtime.md` RT-3 proposed a trait
so the model's two pinned tests could run against the real
`defer_jit_owner`/`drain_deferred_jit_owners` queue, and that it was
deliberately not built on 2026-09-17 because the two tests it would have
served don't need it (one is a pure `WxState` property with no quiescence
dependency; the other is already pinned against production directly). This
lane re-examined that call while reviewing the file and agrees with the
2026-09-17 conclusion for the same reason: production's quiescence argument
(process-wide fast path *plus* per-thread `ThreadQuiescenceEvidence` with
blocked-stack scanning) is materially richer than the model's single
all-or-nothing striped-counter walk, and a trait that made both look
interchangeable would hide that gap rather than close it. If this is revisited,
the right shape is not a shared trait but a **second, independent** model that
encodes the per-thread evidence path and is validated the same way the
existing model is — reusing the existing `CodeCacheLifecycle` type for both
would reintroduce exactly the "reassurance about the simpler design" risk the
header already warns readers about.

## 4. Extend the reg-oop-mask falsification oracle's reach

`vm/src/jit/conservative_roots.rs`'s `reg_oop_mask_oracle` /
`verify_excluded_band_words` machinery (`CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1`)
is the right shape for the risk it covers: every one of the three band-scan
narrowings this file applies (`spill_slot_may_hold_oop`,
`spill_slot_is_dead_above_cursor`, `is_dead_outgoing_reserve`) is a licence to
drop a conservative root, and the oracle is what tells "the drop was a
duplicate the rest of the root set already covers" apart from "the drop was
the only name for a live object" — precisely the false-negative question this
round's FOCUS item 1 asks about. It is off by default and per-process. A
soak/CI job that runs one representative allocation-heavy workload (H2, or
`probes/OopMapWideLocals.java`, both already referenced in this file's own
doc comments as having exercised these paths) with the oracle on and asserts
`ORACLE_UNREACHABLE == 0` and `only-root == 0` at exit would turn "kept as a
ruled-out hypothesis" (the file's own words about `LOCAL_MASK_UNREACHED`) into
a property CI actively defends, rather than one that has merely never yet been
observed to fail.

## 5. Roster-coverage audit for `xt_root_scan.rs`, always-on in debug builds

`take_over_pass`'s "Coverage obligation" doc is explicit that a roster gap is
"silent and fatal" — a peer whose `Rip` is in JIT code but absent from
`live_tids` is a missed conservative root and a live use-after-free. The
module already ships the right instrument
(`audit_roster_covers_jit_peers`, gated on `CRATONVM_XT_ROOT_SCAN_AUDIT=1`)
but it is off by default, including in debug/test builds, because the full
`CreateToolhelp32Snapshot` walk it needs is exactly the per-pass cost the
roster design exists to avoid in production. A debug-only default-on posture
(`cfg(debug_assertions)` rather than release) would catch a roster-coverage
regression in CI's existing test suite without adding any cost to a release
build — the same trade this round's other lanes have made for other
expensive-but-decisive assertions.
