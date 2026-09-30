# An attached host thread runs every JNI function except the Java calls while GC-blocked: its allocations race a pause and are rooted by nothing

> **STATUS (2026-09-29, gce e1/x): KEEP -- unchanged; closed with the package, open by default.** Package rows `*_jniroots_P*` / `*_jniblock_P*` = HotSpot on all three collectors (`verify-e1/ve1`, e1 and base alike); default arm unchanged. **Remaining:** the package flip (cost gate, see `gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`), the `foreign_attach_soak` cfg test and a JNA callback run.

> **STATUS (2026-09-28, gcd d10/j, lane jni10; by reading, no cargo): CLOSED
> WITH THE PACKAGE ON, OPEN WITH THE DEFAULTS; this page's flag can no longer
> be selected in the unsafe combination.** `CRATONVM_JNI_FOREIGN_TRANSITIONS`
> is now on with the package switch `CRATONVM_JNI_NATIVE_TRANSITIONS=1`
> (`vm/src/native/jni.rs::jni_switches`), and REFUSED without indirect locals:
> `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` alone (arm C, one crash in three on d7)
> now leaves the transitions off and prints one
> `[cratonvm] CRATONVM_JNI_FOREIGN_TRANSITIONS is ignored ...` line on stderr.
> Arm D (`CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_FOREIGN_TRANSITIONS=1`)
> is unchanged. No code of the idle attachment's path changed (its leaf
> windows, section 4 of the d5f proposal, are not built). Expected, each of
> `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`, 3 runs,
> `Gcd1JniRootsProbe`: arms D, E, `P="CRATONVM_JNI_NATIVE_TRANSITIONS=1"` the
> seven HotSpot lines ending `PASS all 6`; arm C = arm A's lines, rc 1, never
> 139. Remaining gate unchanged: the package's flip (counted-mutators page,
> d10/j STATUS) plus the `foreign_attach_soak` cfg test and a JNA callback run.
>
> *Previous (d8/x, wave d7, Generational): unchanged -- CLOSED WITH THE FLAGS ON, OPEN WITH THE DEFAULTS.* Arm D (`CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_FOREIGN_TRANSITIONS=1`) and arm E (all three JNI flags) of `Gcd1JniRootsProbe` print HotSpot's seven lines, `PASS all 6`, 3/3 each (`jniroots_D_1..3`, `jniroots_E_1..3`), and arm E with `CRATONVM_XT_ROOT_SCAN_AUDIT=1` 3/3 with 0 `ROSTER HOLE` (`jniroots_E_audit_1..3`). The defaults (arm A) fail `foreign-idle-locals`, `foreign-array-elements` and (2 of 3) `foreign-call-result` 3/3 (`jniroots_A_1..3`). This flag alone (arm C) is not a safe configuration: it crashed once in three (`jniroots_C_1`, rc 139, a raw local read after a move; recorded on `common-w2c-jni-local-refs-are-raw-addresses.md`). Remaining gate unchanged: the three JNI flags flip together under the counted-mutators page's gate (its cost item is missed on d7, see there), plus the `foreign_attach_soak` cfg test and a JNA callback run for this flag.

> **STATUS (2026-09-28, gcd d5/f): CLOSED WITH THE FLAGS ON, OPEN WITH THE
> DEFAULTS (unchanged); flag still default OFF; gate unchanged.** Reviewed
> against d5/f's transition work: the new leaf windows
> (`jni.rs::ForeignJniEntry::enter_leaf`) apply only to a JAVA thread inside a
> JNI native in native (a `NativeCallRecord` in native); an idle attached
> thread's JNIEnv functions still take `ForeignJniEntry::enter_foreign`, the
> full idle -> running -> idle round trip (two deposits per call, the same
> ~microseconds per call d4/k measured for arm E). Extending the windows to an
> idle attachment is the natural next step (record `NativeSync` at
> `foreign_enter_idle`, open a window in `enter_leaf` when the attachment is
> idle and at depth 0) and is written up in
> `gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`.
> New with d5/f, inert with the defaults: a JNIEnv function that C code calls
> from inside a GC-safe FFM downcall (`CRATONVM_FFM_DOWNCALL_GC_SAFE`) leaves
> the downcall's region first (`ForeignJniEntry::enter`, before this page's
> foreign branch).

> **Previous STATUS (2026-09-28, gcd d4/k): CLOSED WITH THE FLAGS ON, OPEN WITH THE
> DEFAULTS; flag still default OFF.** First Linux run of the d3 build
> (Generational, run 1): arm E (`CRATONVM_JNI_INDIRECT_LOCALS=1
> CRATONVM_JNI_FOREIGN_TRANSITIONS=1 CRATONVM_JNI_NATIVE_TRANSITIONS=1`) printed
> HotSpot's seven lines; arms A and B `rc=1`. Arms C and D, runs 2-3 and G1 /
> ZGC are pending.
>
> *What this flag closes* (with `CRATONVM_JNI_INDIRECT_LOCALS` for the
> native's own copies): every heap-touching JNIEnv function of an idle
> attached thread runs as a counted mutator (the audit of all 202 table
> entries in d4/k found none uncounted that touches the heap or decodes a
> handle; see the counted-mutators page), locals made outside a Java call
> live in the attach-level frame until detach, and the attached thread's OS
> tid is published while it runs (d3/k). Retire this page, with
> `gengc-r4w4-rooting2-...` merged into it, when the three JNI flags flip
> together under the gate on
> `gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md` (d4/k STATUS,
> items 1-4; for THIS flag add the `foreign_attach_soak` and a JNA callback
> run, unchanged from w10-c).
>
> *What is left with the default flags* (unchanged since filing): an idle
> attached thread's `NewStringUTF`, `NewByteArray`, `Set*ArrayRegion`,
> `Get*ArrayElements` copy loops, `GetObjectField`, `NewGlobalRef` and every
> other non-`Call*` JNIEnv function run while the thread is excluded from the
> pause (allocation and stores racing a moving collection; a global ref minted
> from a pre-move address), and what they create is rooted by nothing until
> the next Java call.
>
> **Previous STATUS (2026-09-27, gcd d3/k): PARTIALLY FIXED, flag still default OFF
> (not flipped here). Two additions ride on this flag now; the flip gate
> below (d2/i) is unchanged except for arm E.**
>
> * With `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` an attached thread publishes its
>   OS tid while it runs Java or a JNI function and withdraws it while idle
>   (`jni.rs::foreign_leave_idle` / `foreign_enter_idle`,
>   `ThreadRegistry::republish_os_tid_current` / `clear_os_tid`), so the
>   take-over and the helper-window pass cover it:
>   `gcd-d2i-foreign-attached-threads-publish-no-os-tid-20260927.md` (its
>   roster-audit run is part of this flag's flip).
> * The JNIEnv entry this flag introduced (`ForeignJniEntry`) is now also the
>   native -> VM -> native transition of a Java thread inside a JNI native
>   under `CRATONVM_JNI_NATIVE_TRANSITIONS`
>   (`gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`). With that
>   flag off the entry is exactly the d2/i one.
> * **Arm E** of the matrix below, for the native transitions:
>   `E="CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_FOREIGN_TRANSITIONS=1 CRATONVM_JNI_NATIVE_TRANSITIONS=1"`
>   must print HotSpot's seven lines 3/3 on each collector, like D. Add `E` to
>   the `for arm in` list.
> * The HotSpot oracle below was run (gcd d3/k, WSL, OpenJDK 25, the C half
>   compiled with `gcc -Wall`, no warning): the seven lines, 3/3, rc 0; also
>   with the new optional iteration argument `20000`.
>
> Tests for the additions: `cargo test -j 5 -p cratonvm-vm --lib an_attached_thread_publishes_its_os_tid_only_while_it_runs`
> and the five `gcd-d2i-jni-native-methods-are-counted-mutators` tests.
>
> **Previous STATUS (2026-09-27, gcd d2/i): PARTIALLY FIXED, flag still default OFF
> (not flipped here). The out-of-process run that gates the flip is now
> written: `tools/bench/Gcd1JniRootsProbe.java` +
> `tools/probes/jni/Gcd1JniRootsProbe.c`. One adjacent attach-path hang was
> found by reading and fixed (default ON).**
>
> *Build and oracle* (Linux; `JDK=/data/jdkimages/jdk25-linux/jdk-25.0.4+7`
> on vm1):
>
> ```bash
> gcc -O1 -shared -fPIC -pthread -I"$JDK/include" -I"$JDK/include/linux" \
>     -o /tmp/libgcd1jniroots.so tools/probes/jni/Gcd1JniRootsProbe.c
> javac -d /tmp/gcd1jni tools/bench/Gcd1JniRootsProbe.java
> $JDK/bin/java -XX:+UseSerialGC -Xmx64m -cp /tmp/gcd1jni Gcd1JniRootsProbe /tmp/libgcd1jniroots.so
> ```
>
> HotSpot prints exactly these seven lines and exits 0 (verify once; the
> C half was not compiled in the lane, which had no C compiler):
>
> ```
> local-across-upcall: PASS
> local-monitor-across-upcall: PASS
> foreign-attach: PASS
> foreign-idle-locals: PASS
> foreign-call-result: PASS
> foreign-array-elements: PASS
> PASS all 6
> ```
>
> *The matrix* (each cell 3 runs, `-Xmx64m`, for each of
> `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`):
>
> ```bash
> R="timeout 600 cratonvm --java-home $JDK -Xmx64m -cp /tmp/gcd1jni"
> A=""                                                   # defaults
> B="CRATONVM_JNI_INDIRECT_LOCALS=1"
> C="CRATONVM_JNI_FOREIGN_TRANSITIONS=1"
> D="CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_FOREIGN_TRANSITIONS=1"
> for gc in UseGenerationalGC UseG1GC UseZGC; do for arm in A B C D; do for r in 1 2 3; do
>   env ${!arm} CRATONVM_GC_STATS=1 $R -XX:+$gc Gcd1JniRootsProbe /tmp/libgcd1jniroots.so \
>     >/tmp/jni-$gc-$arm-$r.out 2>/tmp/jni-$gc-$arm-$r.err; echo "$gc $arm $r rc=$?"
> done; done; done
> ```
>
> What each arm must print:
>
> | arm | must print | may print (the defect, evidence only) |
> |---|---|---|
> | A defaults | `foreign-attach: PASS` (no hang: rc 0 or 1, never 124) | FAIL on `local-across-upcall` (`code=1`), `local-monitor-across-upcall` (`released=false`), the three `foreign-*` data lines, or a crash |
> | B indirect locals | `local-across-upcall: PASS`, `local-monitor-across-upcall: PASS`, `foreign-attach: PASS` | FAIL on the `foreign-*` data lines (a foreign thread's locals stay raw unless C is also on) |
> | C foreign transitions | `foreign-attach: PASS`, `foreign-call-result: PASS` | FAIL on `foreign-idle-locals` / `foreign-array-elements` (`locals=`/`arrays=` from `IsSameObject`: rooted and remapped in the table, but the native's own copy is raw) and on the two `local-*` lines |
> | D both | HotSpot's seven lines, 3/3 on every collector | nothing |
>
> The flips: THIS flag (`CRATONVM_JNI_FOREIGN_TRANSITIONS`) when C's two
> lines and D's seven are 3/3 on all three collectors AND the JNA callback
> run below is clean; `CRATONVM_JNI_INDIRECT_LOCALS` per its own page
> (`common-w2c-jni-local-refs-are-raw-addresses.md`). A crash, hang (rc 124)
> or new FAIL in B, C or D that A does not show voids the flip. The
> `foreign_attach_soak` (`--cfg foreign_attach_soak`) and a JNA callback run
> with the flag on remain on the list from w10-c (below).
>
> *Found by reading and fixed this wave (default ON, liveness, no flag):*
> `attach_foreign_thread_idle` registered the thread counted and raised
> `in_blocked_region` only after the registry wiring; a pause whose census
> fell in between counted it and waited forever. It now registers STARTING
> and marks it `stw_ready` only after the flag is up (`vm/src/native/jni.rs`,
> `attach_foreign_thread_registered`). Test:
> `cargo test -j 5 -p cratonvm-vm --lib a_starting_foreign_attachment_is_in_no_pause_quota`.
> Also filed: `gcd-d2i-foreign-attached-threads-publish-no-os-tid-20260927.md`
> (no roster entry for an attached thread) and
> `gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md` (a Java
> thread blocked inside a JNI native holds every pause; the probe is written
> around it).
>
> **Previous STATUS (2026-09-26, at `b5c9b6c6e`): PARTIALLY FIXED. The page's fix
> LANDED in gc-common w10-c (`c23ae864c`) behind
> `CRATONVM_JNI_FOREIGN_TRANSITIONS` (token
> `CRATONVM_GC=jni-foreign-transitions`, `types/src/flag_groups.rs:982`),
> default still OFF (`foreign_transitions_active`,
> `vm/src/native/jni.rs:1628`, uses `indirect_locals_from`'s rule: unset is
> OFF).** With the flag off every path is byte-for-byte as before. The design
> held up; nothing in it turned out unsound (see "Why it is sound" below).
>
> Since w10-c: interpreter round i1 wave 19 (`b5c9b6c6e`) made
> `ForeignJniEntry` `pub(crate)` and bound it in the two C `jvmtiEnv`
> functions that decode a handle (`vm/src/jvmti/native_env.rs:1000`,
> `:1032`); the `rg -c` count below is now 129 because one test
> (`jni.rs:16014`) binds it too -- the production JNIEnv count is still 128.
>
> **Open: the default flip, parked on whoever runs the out-of-process
> workloads (Linux host).** Neither the `foreign_attach_soak` nor a JNA
> callback run with the flag on is recorded in any orchestrator verification
> report through w33-w34. Steps at the end of this block.
>
> The w10-c record follows.
>
> What the flag does (`vm/src/native/jni.rs`, section "Per-function
> transition for an idle foreign thread"):
>
> * `ForeignJniEntry` is the first statement of 128 JNIEnv functions: every
>   one that touches the heap, a handle or the pending exception. On a
>   foreign-attached thread at `FOREIGN_CALL_DEPTH == 0` it is the idle ->
>   running -> idle round trip of `ForeignCallGuard` without the per-call
>   frame. Enter: `foreign_leave_idle` (`mark_blocked_region_leave`, which
>   waits out a pause in progress, then `check_post_block_gc`, which applies
>   the fixups the idle window accumulated, including to the JNI local
>   frames). Drop: `foreign_enter_idle` (TLAB retire, `deposit_root_snapshot`,
>   raise `in_blocked_region`, `mark_blocked_region_enter`, arrive if a pause
>   raced in). Nested, and on every other thread, it is inert. The only
>   functions without it are `GetVersion`, `FatalError`, `GetJavaVM`,
>   `ExceptionCheck`, `GetModule`, `ReleaseStringUTFChars`,
>   `ReleaseStringChars` / `GetStringCritical` / `ReleaseStringCritical`
>   (the last two delegate to functions that have it, and the releases touch
>   only a C buffer and the per-VM record table), the `*MethodV` wrappers
>   (each delegates to its `*MethodA` twin) and the invocation-table
>   functions. Check with
>   `rg -c "let _fx = ForeignJniEntry::enter\(\);" vm/src/native/jni.rs`
>   (128 at w10-c; 129 now, the extra one in a test).
> * The attach-level local frame: `attach_foreign_thread` pushes it
>   (`FOREIGN_ATTACH_FRAME`), `detach_foreign_thread` truncates to it. A local
>   made outside a Java call lives there until `DeleteLocalRef` or detach, is
>   published by the deposit at the end of the function that made it, and is
>   remapped by the next leave. A `Call*` made from the attach level runs
>   nested inside its function's entry, so `ForeignCallGuard` pushes no
>   per-call frame and the call's result lands in the attach frame too (it
>   used to be minted AFTER the guard dropped, while idle, recorded nowhere).
> * With `CRATONVM_JNI_INDIRECT_LOCALS` also on, an attach-level local is
>   handed out as an indirect handle: a slot nothing pops before detach, so
>   the native's own copy follows a moving collection. Without it the handle
>   is raw, rooted and remapped in the table, and the native's copy has the
>   `common-w2c` problem every raw local has.
>
> Also found and fixed on the way, default ON (a multi-VM bug fix):
> `ForeignCallGuard`, `DetachCurrentThread`, the re-attach repair path and
> the AIO dispatcher's shutdown resolved the attachment's VM with
> `process_vm()`, the most recently created VM. With a second VM published,
> an attached thread of the first left and re-entered the SECOND VM's blocked
> region. The attachment now records its own VM (`FOREIGN_ATTACH_VM`,
> `foreign_attachment_vm`). Test:
> `foreign_transitions_use_the_attachments_own_vm`.
>
> Tests (`vm/src/native/jni.rs`, `mod tests`), each on a spawned host thread
> attached to a `SharedVm` with its `self_arc`, under `PROCESS_VM_TEST_LOCK`:
>
> * `idle_foreign_jni_objects_survive_a_{generational,g1,zgc}_collection`:
>   `NewStringUTF`, `NewByteArray`, `SetByteArrayRegion` at idle (the thread
>   is idle again after each), two REAL full collections forced from another
>   thread (`force_gc_for_vm`) while the host thread sits idle, then the
>   unchanged handles read back intact (`GetStringUTFChars`,
>   `GetByteArrayRegion`, `GetArrayLength`); detach closes the attach frame.
> * `an_idle_foreign_jni_function_waits_out_a_pause`: another thread holds a
>   stop-the-world pause (the idle thread is excluded from it); the host
>   thread's `NewStringUTF` does not return until the pause completes.
>
> Why it is sound. The one hazard the design has to avoid is an `ObjectRef`
> decoded while idle and used after the leave: the leave can wait out a
> moving pause and remap the table, not a Rust local. The entry is therefore
> the FIRST statement of each function, before any handle is decoded, and it
> stays open for the whole function. Putting it in `with_shared_vm` /
> `with_jni_context`, as the page first suggested, would NOT be sound: many
> functions call them more than once, or decode a handle before them, and
> the thread would go idle (and a collection could run) between two halves
> of one function. A pause requested while the function runs waits for it,
> exactly as it waits for a native method dispatched from Java (a counted
> mutator for the whole call); a function that blocks (`MonitorEnter`
> contended) takes the canonical GC-safe acquire, because the thread is no
> longer in the blocked region. Cost with the flag on: two barrier
> transitions and one root deposit per JNI function on an attached thread
> outside a Java call; the deposit walks the attach frame, so a native that
> never deletes its attach-level locals pays O(locals) per call (HotSpot
> keeps them too, but walks nothing).
>
> Still missing before the default can flip: the `foreign_attach_soak`
> (`--cfg foreign_attach_soak`) and a JNA callback run with
> `CRATONVM_JNI_FOREIGN_TRANSITIONS=1`, then the same two with
> `CRATONVM_JNI_INDIRECT_LOCALS=1` added. When they are clean: flip
> `foreign_transitions_active`'s unset case to ON (keep `0`/`false`/`off`/
> `no` as the kill switch), make the inventory row default-on, and move this
> page to FIXED.

Status: PARTIALLY FIXED (behind `CRATONVM_JNI_FOREIGN_TRANSITIONS`, default OFF)
Area: `vm/src/native/jni.rs` (`ForeignCallGuard`, and every JNI function
that touches the heap)
Filed: 2026-09-24, gc-common round, wave 9, lane G
Backends: all three. Worst on the moving ones: Generational young copy, G1
evacuation, ZGC relocation.

## Evidence

- A foreign-attached thread (a host thread after `AttachCurrentThread`) is
  modelled as GC-blocked while idle. Its `JvmThread` has `in_blocked_region`
  raised, it is excluded from every pause's census, and its roots are its
  deposited snapshot (`ForeignCallGuard` doc; design
  `docs/feature-designs/foreign-thread-attach.md` section 3.3, "Pure-native
  foreign thread (between calls)").
- Only `ForeignCallGuard::enter` turns it back into a counted mutator, and
  only these sites enter it:
  * the three call helpers `jni_call_instance`, `jni_call_nonvirtual` and
    `jni_call_static`, so `Call*Method*`;
  * `jni_new_object_a`, so `NewObject*`;
  * the thread-local release on detach;
  * the AIO dispatcher.

  Check with `rg -n "ForeignCallGuard::enter\(\)" vm/src/native/jni.rs`.
- Every other JNI function runs under `with_shared_vm` / `with_jni_context`
  with no transition. That includes the allocating ones (`NewStringUTF`,
  `NewString`, `New<Type>Array`, `NewObjectArray`, `AllocObject`,
  `NewDirectByteBuffer`), the heap writers (`Set<Type>Field`,
  `SetObjectArrayElement`, `Set<Type>ArrayRegion`, the
  `Release<Type>ArrayElements` copy-back) and the readers.
- A local created by such a call is not rooted. The idle thread has no open
  frame, because `ForeignCallGuard`'s drop truncates its per-call frame, so
  `new_local_handle` records nothing. With a frame the native pushed itself,
  the local is recorded in thread-local storage, but the snapshot the
  collector reads was deposited at the previous guard drop, before the local
  existed.

## Failure scenario

The common callback shape, used by JNA's callback dispatch and by any
library that owns worker threads, goes like this on an attached thread:

```c
jstring s = (*env)->NewStringUTF(env, msg);   // idle: GC-blocked
jbyteArray b = (*env)->NewByteArray(env, n);  // idle: GC-blocked
(*env)->SetByteArrayRegion(env, b, 0, n, buf);
(*env)->CallVoidMethod(env, listener, onEvent, s, b);  // now a mutator
```

1. **The allocation races a pause.** Another thread's stop-the-world
   collection does not wait for this thread, so `NewStringUTF` allocates, and
   `SetByteArrayRegion` writes, while a young copy, G1 evacuation or ZGC
   relocation runs. The object can be placed in space the collector is
   clearing or compacting, or its store can land in from-space after the copy.
2. **The new objects are rooted by nothing until the call.** A collection
   between `NewStringUTF` and `CallVoidMethod` (the thread is excluded, so
   one can start at any time) does not see `s` or `b`. It frees them, or
   moves one and leaves the native's raw handle naming from-space. The
   callback then receives a freed or stale object: a `NoSuchMethodError` on
   `java/lang/Object` (a zeroed header), a wrong string, or a crash.

HotSpot does both halves: every JNI function transitions the thread from
native to VM (blocking for a safepoint in progress), and a local created on
an attached thread outside any native frame lives in the thread's top-level
handle block until `DetachCurrentThread` or `DeleteLocalRef`.

## Proposed fix

In `jni.rs`:

1. **Transition per JNI function.** Every JNI function that touches the heap
   enters a lightweight variant of `ForeignCallGuard` on a foreign thread at
   depth 0. It does the blocked-region leave (wait out any pause, apply the
   fixup map) and, on exit, the TLAB retire, the snapshot deposit and the
   re-entry, but it pushes no per-call frame. Nested calls stay free
   (`FOREIGN_CALL_DEPTH`). A non-foreign thread pays one thread-local check,
   which `ForeignCallGuard::enter` already costs. The natural single place is
   `with_shared_vm` and `with_jni_context`.
2. **An attach-level local frame.** Push one frame in `attach_foreign_thread`
   and close it in `detach_foreign_thread`. Locals created outside a Java
   call land there, are captured by the deposit at the end of the function
   that created them, and are remapped by the next leave's
   `check_post_block_gc`. That is HotSpot's lifetime (until detach or
   `DeleteLocalRef`), and it makes `PushLocalFrame` / `PopLocalFrame` on an
   attached thread behave. `ForeignCallGuard`'s per-call frame keeps scoping
   the locals made inside a Java call.

Put both behind a flag defaulting OFF first, as lane rules require for a
change to the thread-state protocol. Run the `foreign_attach_soak`
(`--cfg foreign_attach_soak`) and a JNA callback test with it before
defaulting it on.

## Confirmation

```bash
rg -n "ForeignCallGuard::enter\(\)" vm/src/native/jni.rs
```

At filing this showed only the call helpers, `NewObjectA`, detach and the AIO
dispatcher (the per-function entry is now `ForeignJniEntry`, see STATUS). A unit test that attaches a thread (under
`PROCESS_VM_TEST_LOCK`), calls `NewStringUTF` at idle, forces a collection
from another thread, and then reads the string back through the handle
would show it freed or stale. With the fix it reads back intact.

## What would retire it

Every heap-touching JNI function on an idle attached thread runs as a
counted mutator, and a local created outside a Java call is rooted until
`DeleteLocalRef` or detach. The attach soak and the unit test above must be
green.
