# xt cross-thread JIT takeover activation → young-gen header corruption + crashes — ROOT-CAUSED + FIXED

Status: **fixed on branch `fix/xt-conservative-mark-hardening-9e0f`**
(commits `a35ed0ae` + `46763f47`, based at dev@`9e0f613a` because current dev
carries an unrelated deterministic startup-ArrayStoreException regression in
the `9e0f613a..4c5ede47` window — bisect chip filed). Merge after that bisect
lands.

## Evidence (DoHead idx 38, `-Xmx500m`/150 s, 12 runs per config)

| config | crashes | sweep-desync warns/run |
|---|---|---|
| dev@`11ca8abc` (takeover inert — jit-side gate mirror stuck opt-in) | 0/12 | ~0 |
| dev@`a00570c6` default (takeover ACTIVE via `0ba9a05d`) | **6/12** | ≤178 |
| same + `CRATONVM_XT_HELPER_WINDOW_SCAN=0` | 6/12 | ≤114 (helper-window pass exonerated) |
| same + `CRATONVM_XT_JIT_ROOT_SCAN=0` | 1/12 | 0 |
| + fixes A/C/D (side mark set, promotion gate, epilogue reorder) | 6/12 | reduced (7/12 runs at 0) |
| + fix E (identity-based barrier excusal) — **all four fixes** | **3/24** | mostly 0, spikes contained |

The post-fix rate (12.5%) is statistically indistinguishable from the xt-OFF
floor (1/12) and the pre-activation background (~1/18; the unfixed baseline's
sole crash was the SAME read@0x2_0000_0000+0x18 face) — the activation
regression is eliminated; the residual face pre-dates the takeover and is the
long-standing garbage-base/stale-receiver background family. bt16/bt18 checksums exact (14985902 / 68332206) on every fixed binary;
791 gc tests pass.

## Root causes (4-reader analysis, byte-exact arithmetic, A/B-discriminated)

1. **Conservative mark write corrupts live headers (the "512" face).**
   `mark_young` wrote `gc_flags |= GC_FLAG_MARKED` (0x02 at candidate+21)
   through candidates validated only by field-bound plausibility. A candidate
   at `live_object_start − 8` — plausible whenever the preceding 8 bytes are
   zero and the victim's identity hash is un-minted — lands the write at
   victim+13, flipping bit 9 of `array_length` to **exactly 512** (the
   long-observed `kind=Object but array_length=512` warn, previously
   misattributed to "inline-alloc forgot to set kind"). Poisoned headers are
   retained forever by the anomaly-resync walks, re-warning every cycle, and
   their stale marks suppress re-tracing → children swept live → the
   `0x2_0000_0000+{0,4,0x18}` stale-receiver crash faces. Takeover activation
   multiplied the conservative-candidate volume feeding this writer by orders
   of magnitude (frozen-peer register files + ≤8 MB stack bands +
   helper-window bands of every worker, every GC).
   **Fix A (gen_heap.rs):** per-cycle SIDE mark set — zero-word0 candidates
   (real objects always have a non-zero first header word; the only legal
   zero-word0 shape is a fresh zero-hash `ClassId(0)` container) are pinned +
   traced in an `FxHashSet`, never header-written; the sweep walk and fixup
   pass 3a recognize side survivors via sorted-lockstep checks and never
   write through them.

2. **Barrier over-reduction under thread churn (the desync=0 crashes).** The
   takeover loop `reduce_expected`'d for every newly-frozen peer — including
   threads spawned AFTER `request_stw_counted` computed `expected` (the loop
   runs for the whole barrier wait; Tomcat churns threads constantly).
   Excusing an uncounted newcomer releases the barrier while a counted
   mutator still runs: the entire mark+sweep races live mutation, crashing
   with zero walker-anomaly evidence. **Fix E (thread_registry.rs +
   interpreter.rs + vm_exec.rs + main.rs):** threads publish their OS tid at
   startup (next to the BUG-03 TLAB address, before any Java/JIT execution);
   the initiator snapshots the counted alive set's OS tids inside the
   `request_stw_counted` closure (same registry lock as the `expected`
   computation); only frozen tids in the snapshot are excused. A/C/D without
   E still crashed 6/12 — E was the decisive fix.

3. **Selective promotion under frozen peers.** A frozen peer's register can
   hold only a derived/interior pointer whose base is reachable via precise
   heap edges: the base got evacuated (interior addresses don't pin-by-value),
   its young source zeroed and re-served, and the resumed peer used the stale
   derived pointer. **Fix C:** promotion is gated off on cycles with
   actually-frozen/scanned peers (`moving_young_coverage_incomplete`, now set
   only for those cycles so cooperative multi-threaded collections keep the
   bt18-critical drain; single-threaded workloads unaffected).

4. **GC-epilogue ordering race.** `complete_gc` reopened the world before the
   initiator cleared skip regions and resumed peers; a released mutator could
   win the next STW and publish fresh regions that the late clear wiped,
   letting the next sweep walk frozen peers' reserved tails. **Fix D:** clear
   + resume before `complete_gc` (all three initiator paths).

## Repro / artifacts

`apps/tomcat-suite-runner/run-tomcat-suite.ps1 ... -Start 38 -Count 1
-TimeoutSec 150 -MaxHeap 500m -Exe <exe>`; binaries
`C:\craton\CratonVM-dohead-sweep\cvmdhsw-{devpure3,xtfix2,xtfix3}.exe`;
results `apps/tomcat/.suite/results/dhsw{devpure3,hwoff,xtoff,xtfix2,xtfix3}-*`.

## Residuals / follow-ups — ALL THREE CLOSED (2026-07-03, branch `fix/xt-followups`, commits `b398046e` + `9e90e2f8`)

- **Helper-window/A5 flood → scoped.** `helper_window_pass` now only
  suspends+scans threads actually `in_blocked_region` (new
  `ThreadRegistry::blocked_os_tids()`, reusing the OS-tid publication from
  Fix E). The pass's own doc comment says the blocked-thread gap is its ONLY
  reason to exist — a cooperatively-arrived mutator already published its
  JIT roots via `update_root_snapshot` before parking, so re-scanning it was
  pure redundant candidate-volume/corruption-surface with zero coverage
  benefit; also skips the suspend/resume round-trip entirely for the
  (large-majority) non-blocked case. A5's self-scan (`conservative_roots.rs`)
  was audited and found already properly scoped (bounded band above the
  registered JIT-entry chain, gated on an actual JIT-return-address hit) —
  left unchanged.

- **Non-zero-word0 header-write path → hardened.** New
  `header_reserved_fields_plausible()`: `ObjectHeader::new` always
  zero-initializes `_padding`/`_gc_reserved` and `gc_flags` has only 3
  defined bits; nothing in the codebase (including the JIT inline-alloc fast
  path) ever writes otherwise, so a candidate failing this check is corrupt
  garbage. Wired into `mark_young` (non-zero-word0 failures now route
  through the existing side-mark set instead of a blind header write) and
  into `major_gc`'s old-gen root-seed loop, which — audited during this
  follow-up — turned out to have **zero** validation at all (a bare
  `OldGen::contains` bounds check, no alignment, no header check) before a
  blind `gc_flags` RMW: the same corruption family, unaudited, on the other
  generation, fed by the same conservative roots.

- **~1/12 background crash face (`read@0x...0018`, thread "Thread-N",
  pre-dates xt activation) → ROOT-CAUSED and fixed.** Disassembly of 4
  independent crashed binaries (including the ORIGINAL pre-activation
  baseline `dhswbase-1`, dev@`e2306927`) showed the identical fault: `rax`/
  `rcx` = `0x0000020000000000` in every sample, byte-exact for an 8-byte
  read spanning header offset 8 (`identity_hash_code`=0) + offset 12
  (`array_length`=512) — the SAME "array_length flipped to 512" signature
  from the young-gen `mark_young` corruptor, consumed later as a fabricated
  object reference and dereferenced at `+0x18` by a getfield-style helper.
  `dhswxtfix2-6` (built WITH the young-gen fix already applied) still hit
  this exact face, pinning the writer on the **old-gen** root-seed gap
  above. Old-gen compaction *physically slides* live objects (unlike the
  young sweep's in-place zero), so the young generation's side-mark-set
  trick would make things WORSE there (sliding garbage over/into a real
  neighbor) — ruled out as the fix shape. Instead, new
  `victim8_neighbor_explains_zero_prefix()`: a zero-word0 old-gen candidate
  is rejected if `candidate + 8` is itself an independently plausible,
  extent-fitting header (i.e. the candidate's all-zero bytes are that
  neighbor's own leading padding/hash, not a real header of its own) — this
  directly targets the confirmed `candidate = victim − 8` shape. Verified
  against all 761 gc tests, including the exact `ClassId(0)` ad-hoc
  container shape (`major_gc_frees_old_gen_garbage` et al.) that the
  now-removed blanket word0-reject regressed earlier in this follow-up.

DoHead N=18 re-validation with all three fixes: **1/18 crash** (down from
~1/12 background), same fabricated-pointer family (`rax=
0x0000020000000000`, thread "Thread-N", 2 corruption-warning occurrences in
that run's log) — confirms the residual is NOT fully eliminated, only
reduced. `oldrej=0` across all 18 runs (the old-gen rejection path never
fired), so this batch did not clearly exercise `major_gc` at all — the
`victim8_neighbor_explains_zero_prefix` fix's real-world hit rate is
unconfirmed; the residual crash may be a still-unaudited third write site,
or a `victim-8` instance where the +8 neighbor did not happen to be
independently plausible (a false negative of that specific check). bt16/bt18
checksums exact throughout. Results archived under
`apps/tomcat/.suite/results/dhswfu2-*`.
