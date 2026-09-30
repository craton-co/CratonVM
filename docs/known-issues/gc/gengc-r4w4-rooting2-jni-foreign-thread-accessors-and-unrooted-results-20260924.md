# JNI on a foreign-attached thread: accessors run outside the pause protocol, and object results are rooted by nothing

> **STATUS (2026-09-29, gce e1/x): KEEP -- unchanged; closed with the package, open by default.** `*_jniroots_P_1..2` and `*_jniroots_P3_1..3` print HotSpot's seven lines on Generational, G1 and ZGC (`verify-e1/ve1`), as on the base. **Remaining:** merge into `common-w9g-...` and retire with the package flip.

> **STATUS (2026-09-28, gcd d10/j, lane jni10; by reading, no cargo): CLOSED
> WITH THE PACKAGE ON, OPEN WITH THE DEFAULTS; the arm-C gate is dropped
> because arm C can no longer be selected.** `CRATONVM_JNI_FOREIGN_TRANSITIONS=1`
> without indirect locals is now refused (`vm/src/native/jni.rs::jni_switches`:
> the transitions stay off, one stderr line), so arm C runs as arm A; the
> package switch `CRATONVM_JNI_NATIVE_TRANSITIONS=1` turns on the combination
> this page needs (foreign transitions + indirect locals). Both findings
> remain closed with it and open with the defaults. Gate: arm D and arm
> `P="CRATONVM_JNI_NATIVE_TRANSITIONS=1"` of `Gcd1JniRootsProbe` print the
> seven HotSpot lines (`foreign-call-result: PASS`, `foreign-array-elements:
> PASS` included) 3/3 on each of `-XX:+UseGenerationalGC` (D done on d7),
> `-XX:+UseG1GC`, `-XX:+UseZGC`; then merge into
> `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md` and retire
> with the package's flip. No code of this page's path changed in d10/j.
>
> *Previous (d8/x, wave d7, Generational): unchanged -- CLOSED WITH THE FOREIGN TRANSITIONS PLUS INDIRECT LOCALS, OPEN WITH THE DEFAULTS; this page's arm-C gate cannot pass and should be dropped.* Arm D of `Gcd1JniRootsProbe` (indirect locals + foreign transitions) prints HotSpot's seven lines, `foreign-call-result: PASS` and `foreign-array-elements: PASS` included, 3/3 (`jniroots_D_1..3`); arm E 3/3. Arm C (`CRATONVM_JNI_FOREIGN_TRANSITIONS=1` alone) fails `foreign-call-result` in both runs that finished (`jniroots_C_2`: `results=1`, `jniroots_C_3`: `results=3`) and crashed once (`jniroots_C_1`, rc 139, a raw local read after a move; see `common-w2c-jni-local-refs-are-raw-addresses.md`), which is what this page's own analysis predicts (the result's raw copy is stale after the first move without indirect locals). So the gate is arm D 3/3 per collector (Generational done), and the page merges into `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md` and retires with the three-flag flip.

> **STATUS (2026-09-28, gcd d5/f): unchanged -- CLOSED WITH
> `CRATONVM_JNI_FOREIGN_TRANSITIONS=1`, OPEN WITH THE DEFAULTS; merge into
> `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md` and retire
> with it.** d5/f's leaf windows and quiet leave do not touch the attached
> thread's path (see the w9g STATUS); both findings stand exactly as below
> with the defaults. Gate unchanged (arm C `foreign-call-result: PASS` 3/3,
> arm D seven lines 3/3 per collector).

> **Previous STATUS (2026-09-28, gcd d4/k): CLOSED WITH `CRATONVM_JNI_FOREIGN_TRANSITIONS=1`,
> OPEN WITH THE DEFAULTS; merge into `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md`
> and retire with it when the JNI flags flip together.** Evidence now on the
> Linux host: arm E of `Gcd1JniRootsProbe` (all three JNI flags) printed
> HotSpot's seven lines on the d3 build (`e2024e4ad`, Generational, run 1),
> including `foreign-call-result: PASS` (finding 2) and
> `foreign-array-elements: PASS` (finding 1); arms A and B `rc=1`. This
> page's own gate (arm C `foreign-call-result: PASS` 3/3 and arm D seven lines
> 3/3 per collector) is pending with the remaining runs.
>
> *Closed by the flag:* finding 1 (every heap-touching JNIEnv function of an
> idle attached thread opens `ForeignJniEntry` first -- re-audited in d4/k
> over all 202 table entries, none missing that touches the heap) and finding
> 2 (a `Call*ObjectMethod*` / `NewObject*` result is minted inside the
> function's entry, while counted, into the attach-level frame).
> *Left with the default flags:* both findings exactly as written below; no
> narrower default-on repair exists (rooting the result needs the attach
> frame, and its raw copy is stale after the first move without indirect
> locals -- the flag package, as d2/i and r5w1 concluded).
>
> **Previous STATUS (2026-09-27, gcd d2/i): still OPEN with the default flags, closed
> by `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` (w10-c); the out-of-process run
> this page asked for now EXISTS and is the retirement criterion. Recommend
> retiring this page INTO `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md`
> when that page's flag flips (unchanged from the r5w1 recommendation).**
>
> *The run* (Linux, the orchestrator's host; this page's own verification
> recipe, written as a probe): `tools/bench/Gcd1JniRootsProbe.java` with the
> native half `tools/probes/jni/Gcd1JniRootsProbe.c`. Its attached pthread
> loops `NewStringUTF` / `NewByteArray` / `SetByteArrayRegion` at idle
> (finding 1), holds a `CallObjectMethodA` result with no global ref across a
> second allocating up-call (finding 2: line `foreign-call-result`), and
> loops `GetIntArrayElements` / `Release` across idle windows (finding 1:
> `foreign-array-elements`), while a Java thread collects beside it. Build,
> oracle and the four-arm flag matrix are on the w9g page's STATUS; for THIS
> page the gate is arm C (`CRATONVM_JNI_FOREIGN_TRANSITIONS=1` alone), which
> must print `foreign-call-result: PASS` 3/3 per collector, and arm D (both
> flags), which must print HotSpot's seven lines 3/3.
>
> *Found by reading this wave, fixed (default ON, liveness):* an idle attach
> (`attach_foreign_thread_idle`, used by `AttachCurrentThread`, the AIO
> dispatcher and the attached services) registered its thread COUNTED and
> raised `in_blocked_region` only after the registry wiring; a pause whose
> census fell in that window counted the thread and waited forever for an
> idle host thread (the `pre_stw` was deliberately ignored). It now registers
> the thread STARTING (`register_starting_with_daemon`), raises the flag, and
> only then `mark_stw_ready` (`vm/src/native/jni.rs`,
> `attach_foreign_thread_registered`; `attach_foreign_thread` keeps its
> behaviour for its direct callers). Test:
> `cargo test -j 5 -p cratonvm-vm --lib a_starting_foreign_attachment_is_in_no_pause_quota`.
>
> The previous STATUS follows, for its point-by-point audit of w10-c.
>
> **Previous STATUS (2026-09-26, gen r5w1/crash5): the proposed fix is ALREADY
> IMPLEMENTED behind `CRATONVM_JNI_FOREIGN_TRANSITIONS` (gc-common w10-c,
> `c23ae864c`), in a sounder form than this page proposed. Both findings are
> closed with the flag ON and open with it OFF (the default), and the default
> is not this page's to flip. No new code for this page; one adjacent
> exit-path defect found and fixed (below). Recommend MERGING this page into
> `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md`, which
> owns the flip.**
>
> Point by point against "Proposed fix" below, verified by reading
> `vm/src/native/jni.rs` at `9e252c8b2`:
>
> 1. *`ForeignCallGuard::enter_accessor()`* -- exists as `ForeignJniEntry`:
>    the leave / `check_post_block_gc` / deposit / re-enter round trip without
>    the per-call frame.
> 2. *Take it in `with_shared_vm`* -- deliberately NOT done, and rightly: many
>    JNI functions call `with_shared_vm` / `with_jni_context` more than once or
>    decode a handle before it, so the thread would go idle (and a moving pause
>    could run) between two halves of one function, with an `ObjectRef`
>    decoded before the leave used after it. Instead `ForeignJniEntry::enter()`
>    is the FIRST statement of 128 JNIEnv functions (`rg -c "let _fx =
>    ForeignJniEntry::enter\(\);" vm/src/native/jni.rs` -> 129 with one test),
>    including `GetPrimitiveArrayCritical`, `Get<Type>ArrayElements`,
>    `AllocObject`, `NewStringUTF` and `GetObjectField`. Finding 1 closed.
> 3. *A base local frame at attach* -- `FOREIGN_ATTACH_FRAME`, pushed by
>    `attach_foreign_thread`, truncated by `detach_foreign_thread`.
> 4. *Hold the guard across the result conversion* -- with the flag on,
>    `jni_call_object_method_a` (and the nonvirtual / static twins, and
>    `jni_new_object_a`) run their `ForeignCallGuard` NESTED inside the
>    function's `ForeignJniEntry`: no per-call frame is pushed, the result is
>    minted by `new_local_handle` while the thread is still a counted mutator,
>    lands in the attach frame, and is published by the entry's deposit.
>    Finding 2 closed.
>
> With the flag OFF (default) both findings stand exactly as written below:
> `jni_call_instance`'s guard drops (pops the per-call frame, deposits, goes
> idle) before the wrapper's `new_local_handle(r)`, which then records into
> no frame. A narrower default-on repair was considered and rejected: rooting
> the result needs the attach frame, which makes the idle snapshot non-empty,
> and without `CRATONVM_JNI_INDIRECT_LOCALS` the native's raw copy is stale
> after the first move anyway (`common-w2c-jni-local-refs-are-raw-addresses.md`)
> -- that is the flag package, not a patch.
>
> **Adjacent defect fixed this wave (default-on, exit path only):** an
> attached thread that EXITS without `DetachCurrentThread` -- or detaches only
> from a `pthread` TSD destructor, after Rust's TLS is gone -- used to free its
> `JvmThread` and leave the registry entry alive, idle-blocked and publishing
> `tlab_addr` / `jvm_thread_addr` into the freed allocation, read by every
> later pause. `ForeignThreadBox`'s destructor now performs the registry half
> of the detach (TLS-free, barrier-serialized) or leaks the box. See
> `../../internal/gc/generational-bytebuf-suite-sigsegv-hashbrown-rehash-FIXED-20260928.md`'s STATUS.
>
> Verification, unchanged from w10-c plus the two new tests:
>
> ```bash
> cargo test -j 5 -p cratonvm-vm --lib idle_foreign_jni_objects_survive
> cargo test -j 5 -p cratonvm-vm --lib an_idle_foreign_jni_function_waits_out_a_pause
> cargo test -j 5 -p cratonvm-vm --lib a_thread_that_exits_attached_is_reaped_not_left_dangling
> cargo test -j 5 -p cratonvm-vm --lib a_thread_exiting_attached_waits_out_a_pause_before_it_is_reaped
> ```
>
> All pass. What would retire this page is the w9g page's default flip, which
> is parked on the `foreign_attach_soak` and a JNA callback run with
> `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` (see that page's STATUS for the exact
> steps). This page's own verification recipe (a pthread looping
> `GetIntArrayElements` while a Java thread churns, holding a
> `CallObjectMethod` result across a second allocating upcall) should be run
> with `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` and expected clean, and without it
> expected to reproduce -- the difference is the evidence for the flip.

*Filed 2026-09-24 by generational GC round 4 wave 4, lane `rooting2`.*

- **Status:** open.
- **Severity:** memory safety for programs whose native threads call back into the VM
  after `AttachCurrentThread` (netty-tcnative callbacks, SQLite/LWJGL worker threads).
- **Backend:** **not generational-specific.** G1 evacuation and the ZGC slide move
  objects under the same window. It is filed here because it was split out of the
  generational hunter page, which only this lane owns.
- **Code:** `vm/src/native/jni.rs`: `with_shared_vm`, `ForeignCallGuard`,
  `jni_call_instance` and the object-returning `Call*` wrappers, `jni_new_object_a`,
  `attach_current_thread_impl`, `jni_detach_current_thread`.

## Where this came from

It was split out of
`docs/internal/gc/gengc-r4w3-hunter-jni-raw-handles-meet-the-moving-young-cycle-RETIRED-20260924.md`.

- That page's item 3 is carried here unchanged.
- The second finding below is new in wave 4.
- The rest of the hunter page was fixed, or duplicates
  `docs/known-issues/gc/common-w2c-jni-local-refs-are-raw-addresses.md` (a raw `jobject`
  held by C code goes stale after any move).

## Findings

1. **Foreign accessors run while the thread counts as GC-blocked.**
   - After `AttachCurrentThread`, the thread sits in the blocked region between guard
     scopes.
   - `ForeignCallGuard::enter` makes it a counted mutator, but only these take it: the
     `Call*` family, `NewObjectA`, the aio dispatcher and thread-local release.
   - Every other accessor goes through `with_shared_vm`, which takes no guard. That
     includes `GetPrimitiveArrayCritical` and `Get<Type>ArrayElements` (their element
     copy loops), `AllocObject`, `NewStringUTF` and `GetObjectField`.
   - So a peer's pause can move or reset from-space halfway through a copy.
2. **Object results of `Call*ObjectMethod*` and `NewObject*` on a foreign-attached thread
   are rooted by nothing** (new, found by reading the code in this wave).
   - `jni_call_instance`'s guard is dropped at the end of that function. Dropping it pops
     the implicit local frame, deposits the root snapshot and blocks the thread.
   - Only after that does the wrapper call `new_local_handle(r)`. No frame is open by
     then, so nothing records the result.
   - `jni_new_object_a` behaves the same way through its own guard.
   - The result can therefore be reclaimed, not just moved, by the next collection.

## Why this was not fixed in wave 4

Both halves change what an attached thread looks like to the collector, and nothing in
this lane could run a real JNI library against them.

- **Finding 1:** every foreign accessor would pay a full blocked-region transition,
  which is expensive for a `Get*ArrayRegion` loop.
- **Finding 2:** the thread would need a base local frame. The idle root snapshot is
  then no longer empty, which the comment at `ForeignCallGuard::enter` says the design
  intends it to be.

## Proposed fix (contained to `vm/src/native/jni.rs`)

1. Add `ForeignCallGuard::enter_accessor()`. It does the same leave /
   `check_post_block_gc` / deposit / re-enter transition as `enter`, but does not push or
   pop the implicit local frame.
2. Take that guard in `with_shared_vm`, so every foreign accessor runs as a counted
   mutator. Nested calls stay depth-only.
3. Give an attached thread a base local frame: push it in `attach_current_thread_impl`
   and pop it in `jni_detach_current_thread` / `release_foreign_thread_locals`.
   - Records made outside any native land there.
   - The idle snapshot roots them and the next leave remaps them.
   - This is HotSpot's model: an attached thread's locals live until Detach.
4. Hold the accessor guard across the result conversion in the three object-returning
   `Call*` wrappers and in `jni_new_object_a`, so `new_local_handle` records while the
   thread is still counted.

**Do not** approximate this with a collector-side "JNI active" divert that bumps the JIT
quiescence counter. That counter is process-wide and labels the cause as JIT. It would
change only the generational young cycle, while G1 and ZGC would keep moving.

## Verification

This needs a JNI test library whose pthread attaches, loops `GetIntArrayElements` /
`Release` while a Java thread churns, and holds a `CallObjectMethod` result across a
second allocating upcall.

- Oracle: HotSpot.
- Run under `-XX:+UseGenerationalGC -XX:+UseG1GC` with
  `CRATONVM_DBG=gc-stress=250000,jni-localref`.
