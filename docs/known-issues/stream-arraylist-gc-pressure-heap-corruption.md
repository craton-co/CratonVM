# Stream/ArrayList under extreme concurrent GC pressure: heap corruption (small-heap only), NOT fixed by the Stream/Comparator ObjectRef pin

Status: OPEN (partially fixed, root cause now strongly suspected — see 2026-07-15 update). Found by
accident 2026-07-14 while verifying dev commit `671c8df3`
("fix(native-collections): pin Stream/Comparator ObjectRefs across GC-triggering calls"). Distinct,
separate residual, not resolved by that fix. **2026-07-14 update:** root-caused and fixed one real,
confirmed instance (`ArrayList.add`/`add(int,Object)`/`addAll` never pinned `this`/the added
element(s) across `al_ensure_capacity`'s internal allocation — see "Fix landed" below), but the crash
still reproduces at `-Xmx32m` after that fix via at least one more, not-yet-pinpointed site.
**2026-07-15 update:** fixed two more confirmed unpinned-across-GC sites (`invoke_virtual`'s lambda
checkcast path, `native_al_stream`'s call into `resync_values_view` — the latter directly contradicts
this doc's own 2026-07-14 "ruled out" note, which missed that `resync_values_view` runs *before*
`native_al_stream`'s own pin), but exhaustive re-testing (30 runs at `-Xmx32m`) shows the corruption
persists at a similar rate, with core dumps recurring at the *exact same* two call sites even after
both fixes — meaning the staleness enters earlier than any of these functions' own bodies. Strong new
evidence (see below) now points at a **known, previously-only-partially-mitigated GC header-race bug
class** ("inline-alloc forgot to set kind=Array", `gen_heap.rs:8862-8880`) as the more likely root
cause, rather than more instances of the simple stale-pin pattern. Repro source is checked in at
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

## Fix landed (2026-07-15, #1): `invoke_virtual`'s lambda-checkcast path used an `args` slice several GC-triggering calls old

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

**Fix**: `vm/src/vm/vm_exec.rs`, `invoke_virtual`'s lambda-dispatch branch — pin every object arg via
the existing `pin_native_object_values`/`reread_native_object_values` helpers (already used elsewhere
in this same file for the identical `<init>`-args hazard) immediately on entering the branch, refresh
into a local `args` before building `full_args`.

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

## Still open after 3 pin fixes: evidence this is NOT (just) more instances of the simple stale-pin pattern

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
wrong by the time it's read — this looks like write-side corruption of the field, not read-side
staleness of a local variable.

Separately, `gen_heap.rs:8862-8880` (`gen_object_total_size`, used by the old-gen non-moving sweep
walker) has an existing, detailed comment describing **exactly this shape** of corruption, already
seen once before in the "binary-trees" workload:

> "a JIT inline-allocation path that writes `array_length` into the header but leaves `kind` at its
> TLAB-zeroed default of `Object`" — logged as `GC: inconsistent header — kind=Object but
> array_length=N (num_slots=N, class_id=N); inline-alloc forgot to set kind=Array` — **exactly** the
> warning text seen in this doc's original 2026-07-14 "Unfixed" repro output.

Critically, that code only makes the **old-gen sweep walker** robust to the corruption (treats it as
"size 0", skips/resyncs past it during a major-GC sweep) — it is a defensive mitigation for one
specific reader, not a fix for whatever JIT/allocator codegen writes the header fields
out-of-order/incompletely in the first place. It does **nothing** to protect ordinary mutator-side
header reads (`get_header`/`kind_of`/`class_id_of`, used by `heap_kind_of` and everything else) from
hitting the same corrupted header directly — which plausibly also explains why
`CRATONVM_DBG_STALE_OBJREF`'s `is_forwarded()` check sometimes fires on **garbage** (a corrupted,
never-fully-initialized header can have random bits that happen to set the "forwarded" flag with a
garbage forwarding address — `get_header`'s panic path dereferences that garbage pointer and
segfaults, rather than printing the clean panic message, exactly the two failure modes this doc's
"Observed" section already listed for the unfixed binary).

Checked and ruled out: the plain-Rust array allocator path actually used by `alloc_ref_array`
(`gc/src/gen_heap.rs:1294`, `GenHeap::try_alloc_array`) builds the *entire* `ObjectHeader` as a local
struct (with `kind` correctly set) and writes it in one `std::ptr::write` — no obvious ordering bug
there. If the header race is real, it's most likely in the **JIT-compiled** bytecode-level
`newarray`/`anewarray` fast path (`jit/src/x64.rs`, `jit/src/aarch64_backend.rs`, and/or the IR
lowering of `Op::NewArray` in `jit/src/ir.rs`) rather than this native-call allocator — plausible here
because the repro's hot loop (24 threads × 3000 iterations) very likely crosses this VM's tiering
threshold and gets JIT-compiled, and `Stream.of(a, b, c)`'s implicit varargs array plus any
autoboxing in the lambda bodies are exactly the kind of array/object allocations that would go through
JIT-emitted code once hot. This has **not** been verified directly (no confirmed backtrace showing a
corrupted object was allocated via JIT-emitted code specifically) — it's the best-supported hypothesis
given (a) the exact log message match to an already-documented JIT-codegen bug class, and (b) ruling
out the native allocator as the source.

## Suggested next steps

1. Confirm or refute the JIT-codegen hypothesis directly: instrument (or manually trace with a
   debugger) whether a corrupted array's allocation site in a failing run was JIT-compiled code vs. a
   native call. `jit/src/x64.rs:21980`'s arraycopy-intrinsic guard documents the header layout
   (`kind` at offset 4, `element_type` at offset 5, `array_length` at offset 12) but is a *reader*, not
   the allocation site — the actual header-*write* codegen for `newarray`/`anewarray` still needs to be
   located and audited for a missing/misordered `kind=Array` store, especially under old-gen-spillover
   or memory-pressure conditions (matches why this only reproduces at `-Xmx32m`, never `-Xmx512m`).
2. Per this repo's established technique for GC-adjacent bugs where post-mortem analysis can't
   distinguish "stale read" from "bad write once the field is already wrong": attach a live gdb
   hardware watchpoint (`watch *(uint8_t*)addr`) on a specific ArrayList's `elementData` field slot (or
   the backing array's own header `kind` byte) across its full lifetime, to catch the exact write that
   corrupts it. A pure post-mortem core dump only shows the state *after* corruption already happened.
3. If (1) confirms a JIT-codegen bug, this is a much larger, riskier fix (raw machine-code emission,
   needs auditing across both the IR-based and single-pass JITs and both x64/aarch64 backends) than
   the three pin/refresh fixes landed so far — treat as a separate, dedicated investigation rather than
   folding it into further "just add one more pin" attempts, which this session's evidence suggests
   will not resolve it.
