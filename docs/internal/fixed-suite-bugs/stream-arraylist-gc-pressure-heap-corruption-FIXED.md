# Stream/ArrayList under extreme concurrent GC pressure: heap corruption (FIXED)

## Resolution (2026-07-16)

Status: FIXED and archived. The 32 MB heap failure combined four GC-safety gaps, not a remaining Stream API semantic defect.

- A full old-generation fallback scan now retains old-to-young edges missed by the card-table fast path.
- A terminal worker publishes its root snapshot before its barrier transition, closing the STW missing-arrival race.
- The non-moving sweep maps aligned conservative interior roots to their exact containing young object before side-marking.
- A derived lazy Stream is rooted and refreshed across close-handler inheritance, which can allocate.

JIT-active young collections use the non-moving path by default because moving collection cannot safely rewrite conservative and derived raw JIT references. CRATONVM_ALLOW_MOVING_YOUNG=1 is diagnostic-only.

Azure validation: two JIT and two no-JIT 24-thread repro runs all reported badSize=0, errors=0, RESULT=OK; cargo test -p cratonvm-gc --lib passed 791 of 791.

## Original investigation


Historical status: OPEN (partially fixed, root cause now strongly suspected — see 2026-07-15 update). Found by
accident 2026-07-14 while verifying dev commit `671c8df3`
("fix(native-collections): pin Stream/Comparator ObjectRefs across GC-triggering calls"). Distinct,
separate residual, not resolved by that fix. **2026-07-14 update:** root-caused and fixed one real,
confirmed instance (`ArrayList.add`/`add(int,Object)`/`addAll` never pinned `this`/the added
element(s) across `al_ensure_capacity`'s internal allocation — see "Fix landed" below), but the crash
still reproduces at `-Xmx32m` after that fix via at least one more, not-yet-pinpointed site.
**2026-07-15 update:** fixed one more confirmed unpinned-across-GC site (`native_al_stream`'s call into
`resync_values_view` — directly contradicts this doc's own 2026-07-14 "ruled out" note, which missed
that `resync_values_view` runs *before* `native_al_stream`'s own pin); a second (`invoke_virtual`'s
lambda checkcast path) turned out to be the identical bug independently found and fixed, more
thoroughly, by a same-day WildFly `parallel-extension-add` investigation — took their fix during the
merge into `dev` (see "Fix landed #1" below). Exhaustive re-testing (30 runs at `-Xmx32m`) shows the
corruption persists at a similar rate regardless, with core dumps recurring at the *exact same* two
call sites even after both fixes — meaning the staleness enters earlier than any of these functions'
own bodies. **This independently cross-confirms** that same WildFly investigation's own deeper
finding (live instrumentation proved stale reads recur *even through the pin-table's "self-healing"
path*): the residual is most likely a **cross-thread GC-root-visibility/timing race** in the pin-table
remap protocol itself, not more instances of the simple stale-pin pattern. A secondary, narrower
hypothesis (a known, previously-only-partially-mitigated GC header-race bug class — "inline-alloc
forgot to set kind=Array", `gen_heap.rs:8862-8880`) is also documented below but not preferred over the
cross-thread-race explanation. Repro source is checked in at
`docs/known-issues/repros/stream-arraylist-gc-pressure/StreamOnlyStressRepro.java`.

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

## Fix landed (2026-07-15, #1): `invoke_virtual`'s lambda-checkcast path used an `args` slice several GC-triggering calls old — superseded by a more thorough independent fix already on `dev`

Got a Linux core dump (Azure host, `core_pattern` + `ulimit -c unlimited`, raw execution not a live
gdb attach, per this repo's established technique) of the post-`8665d1ad` binary still corrupting at
`-Xmx32m`. Backtrace:

```
get_header (gen_heap.rs:1558) <- class_id_of (gen_heap.rs:1580) <- class_id_of (vm_heap.rs:213)
<- lambda_arg_provably_not_instance (interpreter.rs:19024) <- checkcast_lambda_instantiated_args (interpreter.rs:18976)
<- coerce_lambda_args (interpreter.rs:19271) <- invoke_virtual (vm_exec.rs:6850)
<- invoke_deferred_stream_lambda (native-collections/src/lib.rs:12088) <- stream_process_chain (lib.rs:12186)
```

With `CRATONVM_DBG_STALE_OBJREF=1` this backtrace *also* reproduced as a clean panic (not just a
segfault) in a separate run — a second, independent confirmation this is a real stale-`ObjectRef`
dereference, not merely conjecture from a core dump.

`invoke_virtual`'s lambda-dispatch branch built `full_args` (captures + `args`) and handed it straight
to `coerce_lambda_args`/`checkcast_lambda_instantiated_args`, which *does* already pin+refresh
correctly per-argument (see its own extensive existing GC-safety comment) — but only protects against
staleness introduced *after* it receives `args`. By the time `invoke_virtual` reaches that point,
`args` may already be several GC-triggering calls old: the caller's own pin+refresh happened before
`invoke_virtual` was entered, and `load_and_forward`/`recover_stale_lambda_receiver_from_native_pins`/
`lambda_proxies.read()`/`lambda_args_sam_compatible` all run first. `args` is a plain Rust slice,
invisible to the collector, so any GC in that span leaves it stale — and pinning an already-stale
snapshot (which is all `coerce_lambda_args` can do with what it's given) doesn't recover the real
object.

**This is the identical bug** independently found the same day by the WildFly `parallel-extension-add`
investigation — see
[`../internal/fixed-suite-bugs/wildfly-invoke-virtual-lambda-sam-compat-stale-locals-FIXED.md`](../internal/fixed-suite-bugs/wildfly-invoke-virtual-lambda-sam-compat-stale-locals-FIXED.md)
— whose fix is more thorough than the one originally landed here (it pins `receiver`/`args` *before*
`call_site.filter(...)`'s `lambda_args_sam_compatible` call runs, not just before `full_args` is
built afterward, and also refreshes `receiver` in the non-lambda fallback branch). Took their version
during the merge into `dev`, verified via `cargo test -p cratonvm-vm --lib` (2202 passed / 9
pre-existing unrelated failures) rather than re-landing a narrower duplicate.

**Critical follow-up from that investigation, load-bearing for this doc's own residual (see below):**
after landing that fix, temporary diagnostic instrumentation on `checkcast_lambda_instantiated_args`'s
per-argument loop proved the *remaining* stale reads (post-fix, still ~5/12 repro rate) go through the
"self-healing" pinned path (`via_pin=true`) and are **still stale** — ruling out "yet another missed
pin site" for that residual and pointing at a **cross-thread GC-root-visibility/timing race** in the
pin-table remap protocol itself (`thread.native_pin_roots`'s per-thread remap not reliably visible to
that thread's own next read under WildFly's ~37-42 concurrently-executing worker threads, or an
equivalent push-vs-concurrent-GC-cycle race). This independently confirms the same conclusion this
doc's own "Still open after 3 pin fixes" section below reaches from a completely different repro (see
that section).

## Fix landed (2026-07-15, #2): `native_al_stream` called `resync_values_view` on an unpinned receiver

A second core dump (same binary, same repro) hit a different site:

```
get_header (gen_heap.rs:1558) <- kind_of (gen_heap.rs:1585) <- kind_of (vm_heap.rs:213)
<- heap_kind_of (vm_exec.rs:4110) <- al_state (native-collections/src/lib.rs:1684)
<- values_view_source (lib.rs:7312) <- resync_values_view (lib.rs:7225) <- native_al_stream (lib.rs:13547)
```

This directly contradicts this doc's own 2026-07-14 entry, which listed `native_al_stream` as "ruled
out... already correctly pin/refresh their locals across every allocating call." That re-read missed
that `native_al_stream` calls `resync_values_view(ctx, this)` **before** its own
`let this_pin = ctx.pin_native_root(this);` line — the pin was created, but only *after* the risky
call, not before it. `resync_values_view` allocates when resyncing an actual `values()`-view list
(new backing array, or a `SimpleEntry` per element for an entrySet view) and can trigger a moving GC
before `native_al_stream` ever pins anything. (See
[[known-issue-doc-hypothesis-can-be-wrong-not-just-stale]] — this is the same class of miss: a static
"looks fine on skim" verdict that didn't check call order within the function.)

**Fix**: `native-collections/src/lib.rs`, `native_al_stream` — moved the existing
`ctx.pin_native_root(this)` to before the `resync_values_view` call, and refresh via
`ctx.read_native_pin` both before and after it (it can move `this` again via its own allocations).

## Still open after 3 pin fixes: cross-confirms the "cross-thread GC-root-visibility race" finding from the WFLYCTL0079 investigation, on a completely different repro

With both 2026-07-15 fixes applied (`cvfix3-alstream-argpin-20260714.bin`), 30 runs at `-Xmx32m`
(Azure Linux host) still show ~90% failure (segfault or hang) — no better than before these two
fixes, arguably worse (small sample; could be noise, could be the extra pin/refresh overhead shifting
GC timing to expose the race more often). Core dumps from *this* binary recur at **the exact same two
call stacks** listed above (`checkcast_lambda_instantiated_args`'s `class_id_of` and `al_state`'s
`heap_kind_of`), even though both were supposedly closed. `-Xmx512m` still passes clean (0 warnings,
`RESULT=OK`) with all three fixes — no regression, but no improvement at `-Xmx32m` either.

This rules out "just one more unpinned hop, same pattern": both fixes pin+refresh literally at the
top of their functions (`native_al_stream`'s pin is its second statement), so if the crash still
lands in the same spot, the value was already wrong *before* either function was entered — pinning
data that's already corrupt only pins the corruption.

Re-reading the crash mechanics: `al_state`'s `ctx.get_field(this, data_slot)` call that returns the
bad `arr` is a **fresh** read of `this`'s field (not a stale local) — and `al_state`/`values_view_source`/
`al_slots_for`/`unwrap_unmod` all take `&dyn NativeContext` (shared, not `&mut`), so nothing in that
call chain can itself allocate/GC. That means `this.elementData`'s **stored field value** is already
wrong by the time it's read.

**This matches, independently, the `via_pin=true` finding from the same-day `WFLYCTL0079` investigation**
(see the "Fix landed #1" section above and
[`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`](wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md)'s
2026-07-15 follow-up): that investigation proved, with live instrumentation this session didn't have
time to reproduce here, that stale reads recur *even through the pin-table's own "self-healing" path*
— i.e. the pin was created, was current at creation time, and the read that finally used it *still*
observed a stale/reclaimed address. That's a stronger, better-evidenced version of this section's own
conclusion ("the value was already wrong before either function was entered"), and points at the same
root: a **cross-thread GC-root-visibility/timing race** in the pin-table remap protocol — either the
per-thread `native_pin_roots` remap a moving GC performs doesn't reliably reach every thread's own copy
before that thread's next read, or an equivalent race in how a newly-pushed pin becomes visible to a
GC cycle starting concurrently with the push. Two structurally unrelated repros (this one: plain
ArrayList/Stream stress, no WildFly; theirs: WildFly's `parallel-extension-add`, ~37-42 concurrent
worker threads) hitting the identical mechanism is strong corroboration this is a systemic gap, not a
repro-specific coincidence.

Secondary, narrower hypothesis (not ruled in or out, may be a *specific instance* of the above rather
than a separate mechanism): `gen_heap.rs:8862-8880` (`gen_object_total_size`, used by the old-gen
non-moving sweep walker) has an existing, detailed comment describing a matching corruption shape,
already seen once before in the "binary-trees" workload — "a JIT inline-allocation path that writes
`array_length` into the header but leaves `kind` at its TLAB-zeroed default of `Object`", logged as
`GC: inconsistent header — kind=Object but array_length=N...; inline-alloc forgot to set kind=Array` —
**exactly** the warning text in this doc's original 2026-07-14 "Unfixed" repro output. That code only
makes the *old-gen sweep walker* robust to the corruption (skips/resyncs past it during a major-GC
sweep); it does nothing to protect ordinary mutator-side header reads (`get_header`/`kind_of`/
`class_id_of`) from hitting the same corrupted header directly, which would also explain why
`CRATONVM_DBG_STALE_OBJREF`'s `is_forwarded()` check sometimes fires on garbage (a never-fully-
initialized header can have random bits that coincidentally set the "forwarded" flag with a garbage
forwarding address). Checked and ruled out as the *allocator's own* bug: the plain-Rust array
allocator path actually used by `alloc_ref_array` (`gc/src/gen_heap.rs:1294`, `GenHeap::try_alloc_array`)
builds the entire `ObjectHeader` as a local struct (with `kind` correctly set) before a single
`std::ptr::write` — no ordering bug there. If this mechanism is real (as opposed to being just another
manifestation of the cross-thread pin-visibility race above), it's most likely in the JIT-compiled
bytecode-level `newarray`/`anewarray` fast path (`jit/src/x64.rs`, `jit/src/aarch64_backend.rs`, and/or
the IR lowering of `Op::NewArray` in `jit/src/ir.rs`) — not verified directly this session.

## Suggested next steps

Per the WFLYCTL0079 investigation's own conclusion (which this doc's independent evidence now backs),
**diagnosing the exact cross-thread pin-visibility gap is its own dedicated GC/threading-infrastructure
investigation** — not a per-call-site pin audit, which both that investigation and this one have now
shown does not resolve it:

1. Instrument the pin-table remap protocol itself (`gc/src/gen_heap.rs` and whatever cross-thread
   suspend/scan machinery services `thread.native_pin_roots`, outside the JIT-specific
   `vm/src/jit/xt_root_scan.rs` path) to log, per remap cycle, which threads' pin tables were actually
   walked/updated and when — looking for a thread whose pin entry was pushed at (or just after) the
   start of a concurrent remap cycle and never got included in it.
2. Alternatively, per this repo's established technique for GC races that post-mortem analysis can't
   fully explain: attach a live gdb hardware watchpoint (`watch *(uint8_t*)addr`) on a specific
   ArrayList's `elementData` field slot, or the backing array's own header `kind` byte, across its full
   lifetime and across a GC cycle, to catch the exact write (or the exact remap step) that corrupts it.
3. Only pursue the JIT inline-alloc `kind=Array` header-race hypothesis as a distinct investigation if
   (1) or (2) rule out the cross-thread pin-visibility race as the explanation — don't chase both at
   once; they may turn out to be the same underlying gap.
4. Do not attempt further individual "add one more pin" patches for this residual without first trying
   (1) or (2) — this session and the WFLYCTL0079 session both found and fixed a real, confirmed
   unpinned site each, and neither made a measurable dent in the failure rate.
