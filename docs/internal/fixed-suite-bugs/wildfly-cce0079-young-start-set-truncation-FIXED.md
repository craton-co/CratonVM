# FIXED: WildFly boot CCE family root cause — moving young GC's object-start walk silently truncated at the first TLAB gap, dropping every later young object from the forwardable set

**Status: FIXED 2026-07-16** (worktree `/data/wt-cce0079-20260716`, branch
`fix/wildfly-cce0079-close-20260716`, forked from `origin/dev @ dcb24161`).

Closes the root cause of
`docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`
(the `WFLYCTL0079` / `ClassCastException: java.lang.Object cannot be cast to X`
family during `parallel-extension-add`, including the `AttributeAccess`,
`AttributeDefinition`, `Comparable`, `Function`, and `Map` cast-target
variants) — see verification below.

**Timeline correction (learned at merge time)**: the `young_object_starts`
walk this doc root-causes was introduced the SAME MORNING by `1c4aaa06`
("close stream ArrayList pressure corruption") — so the 100%-of-boots
truncation measured here was a same-day regression amplifier on top of the
older, lower-rate CCE family (the 5/12 → 1/19 rates chronicled in the
known-issues doc predate the walk and are the Family-1 long-tail this
branch's ~35 pin fixes + the return-healing barrier address). A concurrent
session independently landed the GAP-filler stride on dev as `fb15be63`;
this branch's walk fix additionally merges the free-list/TLAB skip set,
tracks walk completeness, and adds the sound skip-cycle fail-safe (without
the flag, a corrupt-filler `break` still silently truncates the forwardable
set — the original hazard in a rarer edge).

## Root cause

`gc/src/gen_heap.rs::collect_garbage_inner`, moving (Cheney) young path: the
pre-forwarding `young_object_starts` walk assumed a contiguous bump-allocated
young space —

```rust
while young_cursor < young_used {
    let header = ...;
    let size = gen_object_total_size(header);
    if size < HEADER_SIZE || ... { warn!("stopped at an implausible extent"); break; }
    young_object_starts.insert(obj_ptr as usize);
    young_cursor += size;
}
```

— but young from-space legitimately contains non-object gap ranges:
free-list blocks (from prior non-moving sweeps), reserved TLAB tails, and
**sub-`HEADER_SIZE` GAP-filler sentinels** (`install_tail_filler`'s Bug-D
markers, which carry their byte length at offset 4 and cannot be parsed as
objects). Every OTHER young walk in the file (the non-moving exact walk, the
sweep, both mark walks) strides over all three gap classes; this one did not.
Under WildFly's ~40-thread `parallel-extension-add` the walk reliably hit a
GAP filler within the first few MB (observed: cursor 1.7-24 MB of ~192 MB
used) and **silently `break`'d, leaving every young object above the breakout
point out of the start set — on every single boot** (the
"young object-start walk stopped at an implausible extent" warning appears in
100% of probe logs).

The consequence is in `forward_object_impl`:

```rust
if !young_object_starts.contains(&(old_ptr as usize)) {
    // Exact pre-GC membership rejects aligned interior words from
    // conservative roots before forwarding writes through them.
    return old_ptr;  // ← NOT copied, NOT forwarded
}
```

This membership check — built to reject conservative interior words — also
rejected every genuine young object above the walk breakout, **for every
root** (precise interpreter-frame roots and `native_pin_roots` pins
included) **and every scanned reference slot**. Affected objects were never
evacuated; every reference to them kept the from-space address; the
semispace swap then recycled that memory. Readers subsequently observed
whatever fresh allocation landed there — a valid header for the wrong
object — producing exactly the long-chased symptom set:

- `ClassCastException: java.lang.Object cannot be cast to X` for arbitrary
  `X` (whatever checkcast/`compareTo`/`Map` dispatch touched the recycled
  slot first);
- `via_pin=true` on the 2026-07-15 live captures (the pin machinery worked;
  the pin table itself was remapped through `pointer_map`, but un-forwarded
  objects never entered `pointer_map`, so pinned addresses stayed stale);
- silent boot wedges (corrupted executor/queue state, e.g. stale
  `EnhancedQueueExecutor$PoolThreadNode` links);
- `CRATONVM_DBG_STALE_OBJREF` canary firings at reader sites scattered
  across unrelated natives (the store was fine; the heap-wide forwarding
  pass was incomplete).

Why no prior session found it: the corruption is GC-batch-scale (one broken
cycle poisons an arbitrary subset of the young heap), so every reader-side
capture looked like a distinct "stale local / missed pin" site, and every
per-site pin fix genuinely fixed a real-but-minor bug while the dominant
mechanism persisted. The walk's own warning line was the only direct trace,
and it was buried at WARN level among boot noise.

## Evidence chain (2026-07-16 session)

1. Quarantine-ring capture (see "diagnostics landed" below): a no-JIT domain
   boot with `CRATONVM_DBG_STALE_OBJREF_CYCLES=3` panicked on stale reads in
   `native_map_get` (a HashMap bucket holding a stale key) and interpreted
   `EnhancedQueueExecutor$PoolThreadNode.getTask()` (a node field holding a
   stale ref) — both reads of *heap-resident* stale values, pointing at a
   store- or GC-side mechanism rather than reader-side missed pins.
2. The walk-truncation warning immediately preceded the canary firings and
   appears in **every** baseline probe log (10/10), matching the ~100%
   boot-failure rate of the baseline batch (CCE/hang in 12/12 standalone
   attempts on `dcb24161`).
3. An intermediate build whose fail-safe diverted walk-broken cycles to the
   non-moving sweep still failed — and its failure mode (all-zero-header
   reads on live receivers 145 ms after the first diverted cycle) reproduced
   the documented HIB-CV-22/32/33 behaviour: the non-moving sweep lacks
   conservative over-marking on the precise-root (no-JIT-frames) path and
   reclaims live young objects. This cross-confirmed both mechanisms and
   fixed the fallback choice (skip, don't divert — see below).

## Fix (gc/src/gen_heap.rs)

Three layers:

1. **Walk completion** (the actual fix): the start-set walk now skips the
   same gap classes as every other young walk — merged free-list blocks +
   reserved TLAB tails (`jit_tlab_skip_offsets`) via `skip_free_blocks`, and
   Bug-D GAP-filler sentinels via the offset-4 length stride.
2. **Sound fail-safe**: if the walk still cannot complete (genuinely corrupt
   header), the cycle is **skipped entirely** (empty `GcResult`; young
   over-retains for one cycle and allocation slow paths spill to old gen).
   It is NOT diverted to the non-moving sweep: on this precise-root path
   that sweep reclaims live objects (HIB-CV-22/32/33, re-measured live this
   session).
3. **`run_non_moving_young_cycle` extraction**: the legitimate divert path's
   body (young sweep + threshold-gated old sweep + monitor remap) is now a
   named helper, unchanged in behaviour.

## Diagnostics landed alongside (all default-off)

- `CRATONVM_DBG_STALE_OBJREF_CYCLES=N` — generalizes the stale-ObjectRef
  quarantine from one arena to an N-cycle ring
  (`gc/src/stale_objref_debug.rs`, `quarantine` field), so stale reads
  arriving ≥2 minor GCs late still hit the loud forwarded-header panic
  instead of silently resolving to recycled memory. This is what produced
  the decisive captures. Ring test:
  `gc/tests/stale_objref_quarantine_ring.rs`.
- `CRATONVM_DBG_CCE_BT` — prints receiver identity (class + address,
  `via_pin` where applicable) at ClassCastException construction sites:
  interpreter `checkcast`, `checkcast_lambda_instantiated_args` (permanent
  re-establishment of the 2026-07-15 session's temporary via_pin
  instrumentation), and the native-collections natural-order sites (with
  native backtrace). Complements the existing `CRATONVM_DBG_CCE`
  caller-frame hook.

## Companion wave: native-collections stale-at-store fixes

A systematic audit (this session) of `native-collections/src/lib.rs` found
~25 real, independent Family-1 sites — collection mutators/lookups holding
raw `ObjectRef`s across GC-capable dispatches (`equals`/`hashCode`/
`compareTo`/allocation) — all fixed in the same branch with the established
pin/refresh idiom plus two new GC-safe helpers (`pinned_array_search`,
`ll_pinned_find`). Highlights: `native_tm_put`'s replace branch stored a
stale `value` (and both branches pinned key/value too late);
`pq_sift_up`/`pq_sift_down` swapped pre-dispatch element snapshots through a
stale buffer; `ArrayDeque`/`PriorityQueue`/`LinkedList` add paths stored
pre-GC element addresses after buffer growth/node allocation;
`native_lbq_remove`/`contains` additionally exited a monitor through a
pre-GC receiver (a SynchronizedMethodGuard-class monitor leak);
`al_remove_all`/`retain_all` wrote back a stale `kept` vector (now index
tracked); `hs_add_all`/`remove_all`/`retain_all`/`ll_add_all`/`ad_add_all`
handed stale receivers/elements to reentrant callees; `hs_remove`'s
entrySet/keySet view branches, `lhm_put_evict`'s eviction key,
`remove_source_entry_by_value`, `native_map_keys_as_array`, and the
contains/indexOf/frequency/search lookup family all refreshed through pins.
These are real bugs in their own right (several predate this investigation's
window) but are secondary to the walk truncation for the WildFly CCE rate.

## Verification (2026-07-16, Azure host under heavy shared load 8-45)

Probe harness: `/data/wt-cce0079-20260716/probes/` (run scripts, logs, summary.txt, frozen
binaries `cratonvm-cce0079-{diag1,fix1..fix5}`, SIGSEGV cores).

- **Baseline (dev `dcb24161`, plain JIT standalone, 12 probes)**: CCE markers in 4 of the first 7
  (plus timeouts/hangs; zero OK boots); the truncation warning present in **12/12** logs; a no-JIT
  domain canary probe additionally caught the stale reads live (`native_map_get` bucket,
  `EnhancedQueueExecutor` node field) seconds after the warning.
- **Post-fix standalone (fix2+fix3 binaries, plain JIT, 14 disk-valid probes)**: **0 CCE**;
  3 full `WFLYSRV0025` boots; 4 SIGSEGV (the separately-tracked SB-CRASH-04 JIT oop-map family —
  fresh symbol-bearing cores in `probes/cores/SF2_00{1,6}-core.*`); 6 timeouts attributable to
  host load (no wedge markers); 1 late-boot `VerifyError` (`ManagedScheduledThreadPoolExecutor.reject`,
  different signature, not this family). Later batch tails that coincided with the shared host's
  root/data filesystems flapping to 100% full ("No space left on device" in-log) are excluded as
  environment-invalid.
- **Post-fix domain no-JIT**: probes reliably reach both-servers-registered and the Host
  Controller's own `WFLYSRV0025`; run `DC_001` additionally captured **`Server:server-two`
  `WFLYSRV0025` (started in 92935ms)** with the stale-canary live. A low-rate long-tail of
  further Family-1 producers remains under investigation in the same worktree (tracked in
  `docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`'s
  2026-07-16 section) — each new capture is being closed with the producer-side diagnostics
  landed here.
- **Unit tests** (all `--release`): cratonvm-gc full suite incl. the new ring test — pass;
  cratonvm-native-builtins `--lib` 2999/0; cratonvm-native-io `--lib` 349/0;
  cratonvm-native-collections lib 73/0 + all integration binaries 0 failures. (The
  native-collections and native-io lib-test targets did not even COMPILE on dev — their
  `NativeContext` mocks were missing `resolve_field_index_by_class_id`; fixed here.)
  cratonvm-vm `--lib` carries 7 pre-existing `jit::skip_list` failures from another session
  (verified failing on pristine dev) plus one debug-only lock-order test that cannot pass under
  `--release`; no new failures.
