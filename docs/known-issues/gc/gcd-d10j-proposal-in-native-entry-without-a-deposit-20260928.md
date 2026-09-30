# Proposal: go into native without a full root deposit -- a frozen-frame in-native deposit, and the stack band scanned by the pause

*Filed 2026-09-28 by gcd d10/j (lane jni10), by reading; no build in the lane.
A direction for triage. It is what is left between the JNI in-native package
(`CRATONVM_JNI_NATIVE_TRANSITIONS`, the package switch since d10/j) and its
cost bar on `Gcd1JniCostProbe`'s `noop`.*

## Where things stand after gcd d10/j

With the package on, per JNI native CALL the thread pays (reading
`vm/src/native/jni.rs`):

* on the way in (`native_call_enter_native`): a TLAB retire, ONE full
  `NativeContextImpl::deposit_root_snapshot` (`vm/src/vm/vm_exec.rs`), one
  barrier-lock hold (`GcBarrier::mark_in_native_enter`; the code-reclamation
  band scan `note_blocking_transition_enter` is gone since d10/j);
* on the way out (`native_call_leave_native`): one barrier-lock hold
  (`GcBarrier::leave_in_native_if`, the quiet leave), the `slot_origins` lock.

Per JNIEnv call from a native in native: a leaf function (d5/f) costs two
fences; a function that runs no Java (d10/j, `ForeignJniEntry::enter_vm_only`:
`NewStringUTF`, `NewString`, `New<Prim>Array`, `NewLocalRef`,
`GetObjectField`, `GetObjectArrayElement`, `Get{String,StringUTF}Chars`)
costs two barrier-lock holds and an append of its new locals; every other
function still pays one full deposit on the way back in.

So the `noop` case's floor is the one full deposit per call. By reading, its
parts that scale with the stack, per call, on a compiled caller:

1. **the conservative JIT band scan** (`scan_active_jit_frames` +
   `publish_pinned_jit_roots`): every word from the deposit's SP up to the
   outermost compiled entry -- which includes all the VM's own Rust frames
   between the compiled caller and the JNI dispatch
   (`invoke_on_class_shared_inner` and below, large frames) -- probed with
   `is_object_address`; then the process-global pinned-roots map lock, a
   `HashSet` allocation and a `std::thread::current()` per call;
2. on Generational, **the moving-young coverage proof**
   (`refresh_moving_young_coverage_for_current_thread`): a second walk of the
   same band;
3. **the frame trace** (`capture_published_trace`, a `Vec` with `Arc` clones per
   frame plus the compiled activations), the JMX monitor list and the
   blocked-frame class census, and the class-owner lookups;
4. the per-frame slot scan and the `slot_origins` rebuild (liveness mask per
   frame).

Items 3 and 4 are the same answer call after call when the interpreter frames
did not change -- and below a compiled caller they cannot change between two
native calls: the interpreter frames are suspended callers of the compiled
code, and only a pause (which bumps `NativeSync`), a deoptimisation or a
debugger write changes them. Items 1 and 2 change with the compiled frame's
spill slots, so no memo can serve them; they must stop being paid at entry.

## 1. A frozen-frame in-native deposit (lane owning `vm_exec.rs::deposit_root_snapshot_inner`)

`interpreter::update_root_snapshot` already has a frozen-frame cache
(`JvmThread::rs_cache`, keyed by `Frame::seq`) for the safepoint deposit. Give
the blocking deposit a variant for the in-native entry,
`deposit_root_snapshot_in_native(&mut self, memo: &mut InNativeMemo)`:

* key the memo on `(NativeSync at the last in-native deposit, the frames'
  (seq, pc) list below the top frame, redefinitions_seen)`; a hit reuses the
  previous deposit's frame roots, `slot_origins`, class owners, published
  trace, JMX list and census row for those frames, and scans only the top
  frame;
* a miss is today's deposit, which refills the memo.

`JniNativeCall` would keep the memo on its thread's `NativeCallTls`
(`vm/src/native/jni.rs`, jni lane) and call the variant instead of
`deposit_root_snapshot`. Expected: items 3 and 4 gone from `noop`; items 1
and 2 remain.

## 2. The band scanned by the pause, not by the thread (lanes owning `xt_root_scan.rs` / `gc_quiescence.rs` / `thread_registry.rs`)

HotSpot's thread in native publishes its last Java frame and the collector
walks the frames. The part of that model this VM can adopt without walking
interpreter frames cross-thread: the stack band below the native's C frame
is FROZEN while the thread is in native (the C code cannot return past the
dispatch without leaving native, and leaving waits for the pause). So:

* the in-native entry records the band `[entry SP, outermost compiled entry
  SP)` in the thread's `GcBlockState` (a pair of atomics) instead of scanning
  it, and publishes no pins;
* a pause, for every EXCLUDED thread whose band is recorded, scans the band
  conservatively itself (the take-over's `scan_one_frame` over another
  thread's memory, as the Linux / Windows passes already do for a frozen
  compiled thread), adds the results to that thread's roots and PINS them
  (the words cannot be rewritten, exactly as today's per-deposit pins);
* the moving-young coverage question (item 2) is answered at the pause from
  the same band.

Items 1 and 2 then cost nothing per native call and one band scan per pause
per thread in native. With section 1, the in-native entry becomes a TLAB
retire, the top frame's scan and two barrier-lock holds -- the "state store
plus a poll" of the gcd d10 brief, with the expensive work done only when a
pause is pending.

## 3. Small items (jni lane, after 1 and 2)

* the two barrier-lock holds per bracket become one CAS each on a per-thread
  state word when no pause is in progress (the leaf-window Dekker pair,
  generalised to the in-native state); the lock is taken only when the
  request side is up;
* `JniNativeCall::enter_engaged` clones the VM's `Arc` per call; keep a
  borrowed pointer for the call (the dispatch's `JniContextGuard` keeps the
  VM alive).

## How to verify

`Gcd1JniCostProbe` arm A (defaults) against arm P
(`CRATONVM_JNI_NATIVE_TRANSITIONS=1`, the package), interleaved, 5 runs,
medians of medians: `noop` P/A <= 2, `new-string` P/A <= 5; stdout
`PASS all 4` on both. Correctness rows unchanged: `Gcd1JniRootsProbe` arm P
and `Gcd1JniBlockInNativeProbe` arm P print HotSpot's lines 3/3 on
Generational, G1 and ZGC, and both with `CRATONVM_XT_ROOT_SCAN_AUDIT=1` (0
`ROSTER HOLE`). Before building section 2, a `perf record -g` of arm P's
`noop` case (command on
`gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`'s
d10/j STATUS) confirms the split between items 1-4.
