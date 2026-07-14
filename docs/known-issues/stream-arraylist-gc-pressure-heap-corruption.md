# Stream/ArrayList under extreme concurrent GC pressure: heap corruption (small-heap only), NOT fixed by the Stream/Comparator ObjectRef pin

Status: OPEN (partially fixed) — found by accident 2026-07-14 while verifying dev commit `671c8df3`
("fix(native-collections): pin Stream/Comparator ObjectRefs across GC-triggering calls"). Distinct,
separate residual, not resolved by that fix. **2026-07-14 update:** root-caused and fixed one real,
confirmed instance (`ArrayList.add`/`add(int,Object)`/`addAll` never pinned `this`/the added
element(s) across `al_ensure_capacity`'s internal allocation — see "Fix landed" below), but the crash
still reproduces at `-Xmx32m` after that fix via at least one more, not-yet-pinpointed site. Repro
source is now checked in at `docs/known-issues/repros/stream-arraylist-gc-pressure/StreamOnlyStressRepro.java`.

## Repro

24 threads, each looping 3000 times; each iteration:
1. builds a 30-element `ArrayList<P>` (`particles`)
2. `particles.stream().map(lambda-that-allocates).flatMap(lambda-that-allocates-and-returns-Stream.of(3-strings)).collect(Collectors.toUnmodifiableList())`
3. separately, `particles.stream().map(...).filter(...).findFirst()`

Run under CratonVM real-JDK25 mode with `-Xmx32m` (small heap, to force heavy GC pressure) and
`CRATONVM_DBG_STALE_OBJREF=1`.

Source: `docs/known-issues/repros/stream-arraylist-gc-pressure/StreamOnlyStressRepro.java` (reconstructed
from the description below, since the original was never checked in; passes clean on real HotSpot).

## Observed

Tested two binaries, both **only under `-Xmx32m`**:

- **Unfixed** (dev `220ebb7d`, before the Stream/Comparator pin fix): hard segfault (exit 139, core
  dump), preceded by GC WARN log lines:
  ```
  GC: inconsistent header — kind=Object but array_length=512 (num_slots=0, class_id=0); inline-alloc forgot to set kind=Array
  GC: skipping suspected false root at <addr> (computed size 0 bytes...)
  ```
  This looks like genuine heap corruption, not a simple stale-ref: `CRATONVM_DBG_STALE_OBJREF` did
  **not** cleanly fire here. Either the corruption has a different root cause entirely, or it is a
  stale-ref case whose forwarding-record window had already been reclaimed by the time of the bad read
  (so the assertion has nothing left to catch).

- **Fixed** (with `671c8df3`'s `stream_apply_chain_full` + `native_stream_find_first` lazy-branch
  pinning applied): no segfault, but occasionally a **wrong result** — the test's own
  `IllegalStateException("bad size 0")` assertion fires because
  `particles.stream().map().flatMap().collect(toUnmodifiableList())` silently returns an **empty
  list** instead of the expected 90 elements (30 × 3).

- **Both binaries pass cleanly with zero errors at `-Xmx512m`**, same thread count/iteration count.
  The failure is heap-pressure/GC-timing dependent, not a deterministic logic bug — and it survives
  671c8df3's fix, so it's a separate residual in the same general area (Stream/ArrayList under
  concurrent allocation + moving GC), not something that fix was ever expected to close.

## Hypothesis / where to look

The "silently short/empty result" shape (elements dropped, not garbage) suggests either:
- another unpinned `ObjectRef` somewhere in the `ArrayList.add`/`stream`/`map`/`flatMap` chain that
  goes stale across an allocation, distinct from the two call sites `671c8df3` already fixed, or
- the backing array being read at the wrong point in a concurrent resize/realloc.

The unfixed binary's corruption warning (`kind=Object but array_length=512`, "inline-alloc forgot to
set kind=Array") points at an **inline-allocation fast path** that writes an array's header fields in
the wrong order/incompletely under GC-triggered preemption — worth checking first, since that shape
is specific enough to search for directly (`array_length` header field set before `kind=Array` is
committed).

## Fix landed (2026-07-14): `ArrayList.add`/`add(int,Object)`/`addAll` never pinned across their own resize

Static read of `native-collections/src/lib.rs` found a real, previously-unfixed instance of the exact
"Family 1" stale-`ObjectRef`-across-GC pattern this whole class of bug belongs to: `al_ensure_capacity`
(the shared backing-array-grow helper behind `ArrayList.add`, `add(int,Object)`, and `addAll`) calls
`alloc_ref_array` — a real, GC-triggering allocation — and then writes the new buffer back onto `this`
via `al_set_data`'s `ctx.set_field(this, ...)`, all without ever pinning `this` (or the old backing
array, in the copy branch) first. Every caller has the identical gap on its own copy of `this` (used
again afterward for `al_set_size`) and, in `add`/`add(int,Object)`, on the element being added. This is
by far the hottest allocation site in the repro (~10 resizes per 30-element list × 3000 iterations ×
24 threads). Fixed by pinning `this`/the old buffer inside `al_ensure_capacity` itself, plus pinning
`this`/`elem`/`other_data`/`elems` at each of the three call sites, refreshing after the call returns —
same idiom as `671c8df3` and this week's broader sweep.

**Verified with a real A/B build** (dev tip + this fix vs. dev tip alone, both Windows release builds):
- Pre-fix, a `-Xmx32m` run reliably hit the debug assertion cleanly: `CRATONVM_DBG_STALE_OBJREF: stale
  ObjectRef detected at 0x... — this object was evacuated by a moving GC to 0x... but native/interpreter
  code dereferenced the OLD address`, panicking inside `StreamOnlyStressRepro.lambda$main$0` — a clean,
  unambiguous confirmation of this exact bug class, not previously seen this cleanly for this repro.
- Post-fix, that specific clean panic **no longer occurs** (0 occurrences across repeated `-Xmx32m`
  runs) — this fix is real and eliminates a genuine crash mode.
- **However, the process still hits heap corruption at `-Xmx32m` after this fix**, this time *without*
  a clean assertion fire — instead via the generic defensive guard added by the (unrelated, already
  "✅ FIXED") HIB-CV-32 fix (`gen_heap.rs::read_slot`'s `read_value_checked`): `gen_heap::read_slot:
  corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap
  reference-integrity defect (see HIB-CV-32)`, plus the same `GC: inconsistent header` warnings as
  before, eventually ending in a native crash-handler dump. This is exactly the harder-to-catch case
  this doc originally described ("the assertion didn't fire cleanly... its forwarding-record window had
  already been reclaimed by the time of the bad read") — so **there is at least one more unpinned site
  still to find**, separate from `ArrayList.add`'s family. `-Xmx512m` still passes clean (0 warnings,
  `RESULT=OK`) post-fix, confirming the residual is still heap-pressure-dependent, not a regression.

Ruled out by re-reading (already correctly pin/refresh their locals across every allocating call, per
the same idiom): `native_al_stream`, `stream_apply_chain_full`, `stream_process_chain`,
`stream_pull_internal`/`stream_pull_synthetic_downstream`/`stream_pull_any_downstream`,
`invoke_deferred_stream_lambda`, `stream_make_lazy_derived`. The residual is likely in a path not yet
inspected this session — candidates: the `invokedynamic` string-concatenation path behind
`"a" + p2.id` (used by the `flatMap` lambda's `Stream.of(...)` arguments), `Integer`/`String` boxing
helpers, or another ArrayList-adjacent site not covered by this fix (e.g. `Collectors.toUnmodifiableList`'s
own backing-list construction, distinct from `stream_apply_chain_full`'s per-element pinning).

## Suggested next steps

1. Get a Linux core dump of the still-crashing fixed binary (`kernel.core_pattern` + `ulimit -c
   unlimited`, per this repo's established technique — a live gdb wrapper can suppress GC-timing races,
   so prefer raw execution + post-mortem `gdb <binary> <core>` over attaching live) and get a real
   backtrace for the corruption that survives this fix.
2. Since the debug assertion doesn't fire cleanly for this residual, consider instrumenting
   `gen_heap.rs::read_slot`'s `read_value_checked` fallback (the HIB-CV-32 guard that's currently
   catching this) to log the corrupt cell's *reader* call site/backtrace, not just the raw bytes — that
   would turn this into the same kind of clean, attributable signal the `ArrayList.add` fix above got
   from `CRATONVM_DBG_STALE_OBJREF`.
3. Audit the `invokedynamic` string-concat bootstrap and any other native helper the `flatMap`
   lambda's `Stream.of("a"+p2.id, ...)` touches for the same unpinned-across-allocation shape.
