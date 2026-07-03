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

## Residuals / follow-ups

- ~1/12 background crash face persists with takeover disabled too (pre-dates
  activation; the classic garbage-base family).
- The helper-window pass and A5 band scans still flood roots with
  conservative candidates each GC (perf/over-retention) — consider scoping to
  blocked-thread stacks only.
- Rare non-zero-word0 fake candidates can still take the header-write path
  (garbage predecessor bytes passing kind/num_slots bounds) — orders of
  magnitude rarer than the zero case; an allocation-start bitmap remains the
  structural end-state.
