# Proposal: make going INTO native cheap -- a light in-native deposit, an incremental re-entry, and FFM downcalls on the JNI in-native state

> **STATUS (2026-09-28, gcd d10/j, lane jni10; by reading, no cargo in the
> lane): SECTION 2 BUILT, SECTION 1 PARTLY (the part in the JNI files), 3 and
> 4 NOT BUILT; still opt-in. The `new-string` bar is expected met, the `noop`
> bar is NOT (one full deposit per native call remains); the rest of section
> 1 is re-scoped into `gcd-d10j-proposal-in-native-entry-without-a-deposit-20260928.md`.**
>
> *Where arm E's microseconds went (by reading; d7: `noop` 4797 ns, `new-string`
> 9097 ns against 526 / 502 on arm A).* Per in-native BRACKET: one full
> `deposit_root_snapshot` (its stack-proportional parts: the conservative JIT
> band scan from the deposit's SP to the outermost compiled entry -- the VM's
> own Rust dispatch frames included -- plus `publish_pinned_jit_roots`' global
> lock and allocation; on Generational a second walk of the same band for the
> moving-young coverage proof; the frame trace, JMX list, class owners and
> `slot_origins` rebuild), a THIRD band walk for code reclamation
> (`note_blocking_transition_enter` in `mark_blocked_region_enter`), and three
> barrier-lock round trips. Not an OS-tid publish, not a take-over signal
> (neither is on the per-call path). `new-string` paid the full bracket per
> `NewStringUTF`; `noop` per native call.
>
> *What landed (`vm/src/native/jni.rs`, `vm/src/threading/gc_barrier.rs`; all
> inert unless the JNI in-native package is on):*
>
> 1. **Section 2, the incremental re-entry:** `ForeignJniEntry::enter_vm_only`
>    for `NewStringUTF`, `NewString`, `New<Prim>Array`, `NewLocalRef`,
>    `GetObjectField`, `GetObjectArrayElement`, `GetStringUTFChars`,
>    `GetStringChars` and every leaf function that cannot open a window
>    (`enter_leaf`'s fallback). A quiet leave keeps `slot_origins`
>    (`native_call_leave_native(.., keep_origins)`), the entry owns the new
>    `LEAF_VM_ONLY` state, and its drop re-enters through
>    `native_call_reenter_incremental` -- TLAB retire, SATB flush, APPEND the
>    locals minted since the leave (`LocalFramesMark`,
>    `collect_local_ref_roots_since`), flag up, barrier enter -- when the
>    function reached no `with_jni_context` (the door of every raise and every
>    Java run, which marks the entry `LEAF_VM_ONLY_ESCALATED`), `NativeSync` is
>    unchanged, no frame was converted (`NativeCallRecord::deposit_redefinitions`)
>    and fewer than `INCREMENTAL_APPEND_LIMIT` (256) entries were appended since
>    the last full deposit. Otherwise the full deposit, as before. The
>    off-frame roots are not re-appended: a vm-only function's ordinary path
>    writes no `JvmThread` field (the one a JNIEnv function writes,
>    `native_pending_return`, goes through `with_jni_context`).
> 2. **Section 1, the JNI-file part:** no in-native bracket publishes the
>    code-reclamation blocked-stack summary any more
>    (`GcBarrier::mark_in_native_enter`; the drain then treats the thread as
>    running, as with the flag off), and the leave's counter release and flag
>    clear are ONE barrier-lock hold when no pause is in progress
>    (`GcBarrier::leave_in_native_if`; with a pause in progress it is the old
>    two-step). The deposit itself (`vm_exec.rs::deposit_root_snapshot_inner`,
>    not this lane's) is unchanged: see the d10/j proposal above for the
>    frozen-frame deposit and the band scanned by the pause.
> 3. **The package** (brief item 2): `CRATONVM_JNI_NATIVE_TRANSITIONS=1` alone
>    now turns on all three JNI flags (`jni.rs::jni_switches`), each other
>    flag's explicit `0` still wins, and the foreign transitions are refused
>    without indirect locals (arm C; one stderr line). Default unchanged: all
>    OFF.
>
> *Commands (orchestrator; each collector):* build lines in
> `tools/bench/Gcd1JniCostProbe.java`'s class comment; then for `gc` in
> `UseGenerationalGC UseG1GC UseZGC`, 5 runs, arms interleaved per run:
> `A=""`, `B="CRATONVM_JNI_INDIRECT_LOCALS=1"`,
> `E="CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_FOREIGN_TRANSITIONS=1 CRATONVM_JNI_NATIVE_TRANSITIONS=1"`,
> `P="CRATONVM_JNI_NATIVE_TRANSITIONS=1"` (the package alone),
> `env $ARM cratonvm --java-home $JDK -XX:+$gc -Xmx256m -cp /tmp/gcd1jnicost Gcd1JniCostProbe /tmp/libgcd1jnicost.so`.
> Expected stdout on every arm: `noop: ok`, `array-length: ok`,
> `int-region: ok`, `new-string: ok`, `PASS all 4`. Expected medians: P equal
> to E within noise; `new-string` E/A <= 5 (expected ~1.2-2x: one full
> deposit per 256 rounds); `array-length`, `int-region` unchanged from d7
> (1.3-1.5x); `noop` E/A still ABOVE 2 (expected below d7's 9x: two band
> walks and two lock holds fewer per call, the deposit's remain). Split of
> what remains: `CRATONVM_JNI_NATIVE_TRANSITIONS=1 perf record -g -o /tmp/jc.perf -- cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx256m -cp /tmp/gcd1jnicost Gcd1JniCostProbe /tmp/libgcd1jnicost.so`
> then `perf report -i /tmp/jc.perf --no-children --sort symbol | head -40`.
>
> **Gate (unchanged in substance):** E (= P) within 2x of A on `noop` and 5x on
> `new-string`, correctness rows unchanged; then the package flips by making
> `jni_switches_from`'s unset `native` answer `true` (kill switch
> `CRATONVM_JNI_NATIVE_TRANSITIONS=0`). Remaining owner: the lane owning
> `vm_exec.rs::deposit_root_snapshot_inner` (d10/j proposal section 1) and the
> take-over lanes (section 2). Sections 3 (FFM downcalls) and 4 (idle
> attachments' leaf windows) of this page are untouched.
>
> *Previous (d8/y triage): KEEP, rank 7 of 54; d7 cost rows jnicost_A/E_1..5 as
> quoted above; correctness rows jniblock_D_1..3, jniroots_D/E_1..3 and both
> `_audit` rows print HotSpot's lines.*

*Filed 2026-09-28 by gcd d5/f (lane native5). A direction for triage, not a
defect fix. Follows `../../internal/gc/gcd-d3k-proposal-signal-free-in-native-threads-REJECTED-20260928.md`
(whose item 2 d5/f implemented in part) and the d5/f STATUS on
`gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`.*

## Where things stand after gcd d5/f

With `CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_NATIVE_TRANSITIONS=1`:

* a LEAF JNIEnv call from a native in native costs two fences and a few
  thread-local reads (leaf windows, `ForeignJniEntry::enter_leaf`);
* LEAVING native (the end of a native call, the start of a non-leaf JNIEnv
  call) is a barrier lock round trip when nothing happened in the window (the
  quiet leave, `native_call_leave_native`);
* ENTERING native -- every native call, and the return of every non-leaf
  JNIEnv call -- is still one full `deposit_root_snapshot`: interpreter frames
  (typed-map scan per frame), frame class owners, the deopt stash,
  `slot_origins`, off-frame thread roots, JNI local frames, the moving-young
  coverage proof (`refresh_moving_young_coverage_for_current_thread`, a full
  native-stack band walk on Generational), the conservative JIT scan and pin
  publication, the frame-trace capture (an `Arc` clone per frame), the JMX
  monitor list, the obsolete-frame census and the reclaimed-slot audit; plus
  `note_blocking_transition_enter`'s JIT band scan and a TLAB retire. That is
  what is left of `noop` (~3 us expected) and of `new-string` (one per
  `NewStringUTF`).

## 1. A light deposit for going into native (lane owning `vm_exec.rs::deposit_root_snapshot_inner`)

A thread going into native is RUNNABLE and comes back within the same frame;
several parts of the blocking deposit serve a PARKED thread's observers:

* the reclaimed-slot audit (`audit_frames_for_reclaimed_slots`) -- a
  diagnostic; gate it on its flag for this kind;
* the JMX locked-monitor list and the frame trace -- read only while the
  thread is blocked (`ThreadInfo`, `Thread.getStackTrace` of a blocked peer).
  A thread in native IS read that way (a frame-trace pause excludes it), so
  keep them, but make them lazy: publish a "trace owed" bit instead, and let
  the frame-trace pause (which runs while the thread cannot leave native)
  build the trace from the thread's frames -- only possible if the pause can
  read the frames of an excluded thread, which is exactly what
  `debugger_inspect` already does for JDWP;
* the moving-young coverage refresh: a thread in native re-proves the same
  chain it proved at its previous deposit unless its JIT entry chain changed;
  memoise the proof on `(JIT entry-chain generation, stack band)`.

Step 0 is measurement: a per-part timer in the deposit behind a debug flag,
run on `Gcd1JniCostProbe` arm E's `noop`.

## 2. An incremental re-entry after a non-leaf JNIEnv call (native5's region)

After an allocating JNIEnv function that runs no Java (`NewStringUTF`,
`NewString`, `New<Prim>Array`, `NewLocalRef`, `GetObjectField`,
`GetObjectArrayElement`, `GetObjectClass`, ...), with `NativeSync`
unchanged (no pause, no fold since the last deposit), the frames, class
owners, deopt stash and JIT part of the snapshot are exactly as deposited.
What changed: new JNI locals, the TLAB, perhaps the thread's
`native_pending_return` (an exception). So the re-entry can be: retire the
TLAB, flush SATB, APPEND the referents of the locals created in the call (the
top frame's slots past the length recorded at the leave) and the off-frame
roots to the thread's `root_snapshot`, raise the flag, enter the blocked
region -- and fall back to the full deposit every N appends (N ~ 64) so the
snapshot cannot grow without bound in a `NewStringUTF`/`DeleteLocalRef` loop
(a deleted local's referent is only over-retained until then). A function
that can run Java (`Call*`, `NewObject*`, `ThrowNew`, `MonitorEnter`
contended, `FindClass`, ...) keeps the full path. The classification is a
second `ForeignJniEntry` constructor, like `enter_leaf`. Expected
`new-string` on arm E: well under 1 us.

## 3. FFM downcalls on the JNI in-native state (`CRATONVM_FFM_DOWNCALL_GC_SAFE`)

Round 13's stopgap runs a downcall in `begin_blocking_region` /
`end_blocking_region`: reported `WAITING`, a JDWP inspection window, and a
full wake with a second deposit on the way out. `native-builtins` cannot reach
`native::jni`'s in-native entry and quiet leave, only `NativeContext`
methods. The exact edits:

`native-api/src/registry.rs` (`NativeThreadAccess`, after
`end_blocking_region_refs`; no lane owns it this wave -- orchestrator):

```rust
    /// gcd d5/f proposal: enter the IN-NATIVE state for a foreign call (an
    /// FFM downcall): GC-safe like a blocking region, but `Thread.getState()`
    /// stays RUNNABLE and the leave skips the wake's work when nothing
    /// happened. Pair with exactly one `end_native_call`. The default is the
    /// blocking region, so any `NativeContext` works.
    fn begin_native_call(&mut self) {
        self.begin_blocking_region();
    }

    /// End what `begin_native_call` began.
    fn end_native_call(&mut self) {
        self.end_blocking_region();
    }
```

`vm/src/vm/vm_exec.rs` (`impl NativeThreadAccess for NativeContextImpl`,
lane u's region):

```rust
    fn begin_native_call(&mut self) {
        crate::native::jni::foreign_call_enter_native(self.shared, self.thread);
    }

    fn end_native_call(&mut self) {
        crate::native::jni::foreign_call_leave_native(self.shared, self.thread);
    }
```

`vm/src/native/jni.rs` (native5): `foreign_call_enter_native` /
`foreign_call_leave_native` install / remove a `NativeCallRecord` for the
calling thread exactly as `JniNativeCall::enter_engaged` / `drop` do, from the
`(shared, thread)` they are given instead of the JNI thread-locals (an FFM
downcall holds no JNI local, so no indirection precondition), so leaf windows
and the quiet leave apply unchanged, and a JNIEnv call from the C code takes
`enter_from_native`. `native-builtins/src/panama.rs` then calls
`ctx.begin_native_call()` / `ctx.end_native_call()` instead of the blocking
region, and `panama_libffi::leave_downcall_region` /
`reenter_downcall_region` call them for upcalls (`upcall_entry` already goes
through those two helpers since d5/f, so that is a two-line change in
`panama_libffi.rs`). With the in-native entry still one full deposit per
downcall (section 1), the flip of `CRATONVM_FFM_DOWNCALL_GC_SAFE` waits for
section 1 too; `Gcd5FfmDowncallBlocksGcProbe`'s stderr gives the A/B.

## 4. Leaf windows for an idle foreign attachment

An idle attached thread (`CRATONVM_JNI_FOREIGN_TRANSITIONS=1`) is excluded
exactly like a native in native, and its leaf JNIEnv calls pay the full idle ->
running -> idle round trip. Record a `NativeSync` at `foreign_enter_idle`
(after its deposit) in the attachment's thread-local state, and let
`ForeignJniEntry::enter_leaf` open a window for an idle attachment at depth 0
whose attach-level locals are indirect, under the same checks. The drain,
the escalation and the Dekker argument are unchanged.

## What it would buy

Sections 1 + 2 bring `noop` and `new-string` on the flipped arm near the
default arm, the last cost blocker of the three JNI flags' flip; section 3
makes FFM downcalls HotSpot-shaped (RUNNABLE, never stalling a pause) at a
cost that can be defaulted.

## 5. Merged from `gcd-d3k-proposal-signal-free-in-native-threads` (d8/y, 2026-09-28): no take-over signal for a thread in native

Retired as a duplicate; its item 2 is section 2 above. Its item 1: with the
native transitions on, a thread inside a JNI native is in the blocked
population, yet the take-over still signals (Linux) or suspends (Windows) it
on every pass, because its roster is `ThreadRegistry::alive_count_and_os_tids`
(blocked threads included), and the helper-window pass signals every blocked
thread again whenever JIT code is loaded. `nanosleep`, `poll`, `epoll_wait`,
`select` and `sem_timedwait` return `EINTR` whatever `SA_RESTART` says, so C
code that treats `EINTR` as an error misbehaves only here; HotSpot never
signals a thread in native for a safepoint. The change: a take-over roster
without blocked entries (keeping the COUNT as it is), and a flag-raising
deposit that records "no compiled frame on this stack" so the helper-window
roster can skip such a thread. Opt-in first; gate: the audited soak of
`common-c-linux-takeover-signals-every-thread-FIXED-20260929.md` (no `ROSTER HOLE`, coverage
accounting unchanged) and a JNI-heavy benchmark.

## 6. Merged from `gcd-d2i-proposal-one-in-native-state-for-jni` (d8/y, 2026-09-28): the flip this page's work unblocks

Retired as a duplicate. Its step 1 (one in-native state for Java threads in a
JNI native, reusing `ForeignJniEntry` with a generalised predicate) is built
behind `CRATONVM_JNI_NATIVE_TRANSITIONS` + `CRATONVM_JNI_INDIRECT_LOCALS`
(gcd d3/k) and cheapened by the leaf windows (gcd d5/f). What it still asks:

1. run the strict JNI corpus, netty-tcnative, JNA and lz4/zstd with the three
   JNI flags on (the d7 probe rows are green: jniblock_D, jniroots_D/E and
   both `_audit` rows print HotSpot's lines);
2. flip `CRATONVM_JNI_NATIVE_TRANSITIONS`, `CRATONVM_JNI_FOREIGN_TRANSITIONS`
   and `CRATONVM_JNI_INDIRECT_LOCALS` together (they become one switch) once
   sections 1-2 above meet the cost bar;
3. then delete `host_thread_enter_native`'s bespoke path: the launcher's
   coordinator thread uses the same in-native state.
