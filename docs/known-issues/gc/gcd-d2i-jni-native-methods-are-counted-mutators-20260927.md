# A Java thread inside a JNI native method is a counted mutator: a native that blocks in C holds every pause

> **STATUS (2026-09-29, gce e1/x): KEEP -- the package is correct, the cost gate still fails.**
> - Correctness (`verify-e1/ve1`, e1): `*_jniroots_P`, `_P3`, `_E` and `*_jniblock_P`, `_P3` = HotSpot on Generational, G1 and ZGC; the frozen-band memo audit rows `*_jnimemo_audit` = HotSpot on all three, and Generational reported `[GC] unreg_memo: shortcircuits=1425527 SUPPRESSED=0` (memo engaged and sound).
> - Cost: package `noop` did not move (5.1-7.3 us on e1 against 5.3-6.8 us on base; default arm about 0.8 us). The whole-stack probe was not the cost.
> - Follow-up JNI e1b (orchestrator): it halved the Generational package cost but regressed the DEFAULT arm on G1 and ZGC.
> - **Remaining:** fix e1b's G1/ZGC default-arm regression; P/A <= 2 on `noop` on all three collectors (the `CRATONVM_DBG_JNI_PHASE=1` census names the phase), then the flip.

> **STATUS (2026-09-29, gce e1/j): OPEN -- the package is correct on all
> three collectors (gcd d10 verification); its `noop` cost item is narrowed
> by a fix in code that awaits the probe. No default changed, and the flip is
> the orchestrator's.**
>
> - Newly found: the dominant per-call cost is not what the d10/j proposal
>   lists. Each in-native deposit probed the WHOLE stack above the outermost
>   compiled entry for a JIT return address, and did so twice on
>   Generational.
> - Fixed behind the package only, by a frozen-band memo. See
>   `gce-e1j-in-native-deposit-rescans-the-whole-stack-above-the-jit-chain-20260929.md`
>   (`vm/src/jit/conservative_roots.rs`: `with_frozen_band_memo` /
>   `frozen_band_clean`; `vm/src/native/jni.rs`: `native_call_enter_native`).
> - Measure `Gcd1JniCostProbe` arms A and P, interleaved, 5 runs per
>   collector. The commands and expected numbers are in section 2 of
>   `docs/internal/gc-design-perf-round-20260929/e1-j-report.md`. The bar is
>   P/A <= 2 on `noop`.
> - If P/A is still above 2, the next cut is section 2 of
>   `gcd-d10j-proposal-in-native-entry-without-a-deposit-20260928.md`, then
>   `publish_pinned_jit_roots`' global lock per deposit (report, section 5).

> **STATUS (2026-09-28, gcd d10/j, lane jni10; by reading, no cargo): FIXED
> BEHIND ONE OPT-IN PACKAGE SWITCH; the `new-string` cost is expected under
> its bar, the `noop` cost is NOT (one full deposit per native call); no
> default changed.**
>
> - **One switch (brief item 2):** `CRATONVM_JNI_NATIVE_TRANSITIONS=1` (token
>   `CRATONVM_GC=jni-native-transitions`) now turns on the whole package --
>   native transitions, indirect locals, foreign transitions
>   (`vm/src/native/jni.rs::jni_switches` / `jni_switches_from`); an explicit
>   `CRATONVM_JNI_INDIRECT_LOCALS=0` or `CRATONVM_JNI_FOREIGN_TRANSITIONS=0`
>   still wins. The foreign transitions are REFUSED without indirect locals
>   (arm C of the roots probe, the one that crashed: `jniroots_C_1`, rc 139);
>   a refused `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` prints one
>   `[cratonvm] CRATONVM_JNI_FOREIGN_TRANSITIONS is ignored ...` line on
>   stderr. All three unset (the default) is all three OFF, byte-for-byte as
>   before. The flip, once the cost bar is met, is one line (`jni_switches_from`:
>   `native.unwrap_or(true)`), kill switch `CRATONVM_JNI_NATIVE_TRANSITIONS=0`.
> - **Cost (item 4):** the incremental re-entry after a function that runs no
>   Java (`ForeignJniEntry::enter_vm_only`), the single-lock quiet leave
>   (`GcBarrier::leave_in_native_if`) and no code-reclamation band scan per
>   bracket (`GcBarrier::mark_in_native_enter`); details and the remaining
>   `noop` floor on `gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`
>   (d10/j STATUS) and `gcd-d10j-proposal-in-native-entry-without-a-deposit-20260928.md`.
> - **Probe matrix to run (orchestrator), each of `-XX:+UseGenerationalGC`,
>   `-XX:+UseG1GC`, `-XX:+UseZGC`, 3 runs** (build lines in the probes' class
>   comments; `P="CRATONVM_JNI_NATIVE_TRANSITIONS=1"`, the package alone;
>   A/B/C/D/E as below on this page):
>   * `Gcd1JniBlockInNativeProbe`: arms D, E and P print HotSpot's four lines
>     (`block-in-native: PASS`, `local-across-block: PASS`,
>     `poll-in-native: PASS`, `PASS all 3`), rc 0; arm C
>     (`CRATONVM_JNI_NATIVE_TRANSITIONS=1` alone) is now the package too and
>     must print the same four lines (it failed by design before d10/j); arms A
>     and B fail `block-in-native` in ~30 s, rc 1, never 124, as before;
>   * `Gcd1JniRootsProbe`: arms D, E and P print HotSpot's seven lines ending
>     `PASS all 6`; arm C (`CRATONVM_JNI_FOREIGN_TRANSITIONS=1` alone) now
>     behaves as arm A (the refusal line on stderr, the foreign lines FAIL, rc
>     1) and must NEVER crash (rc 139 voids the change);
>   * both probes with arm P plus `CRATONVM_XT_ROOT_SCAN_AUDIT=1`: 0
>     `ROSTER HOLE`;
>   * `Gcd1JniCostProbe` arms A/B/E/P, 5 runs: see the d5f proposal's d10/j
>     STATUS for the expected medians.
> - **Unit tests:** `cargo test -j 5 -p cratonvm-vm --lib a_vm_only_jni_function_reenters_native_without_a_full_deposit`,
>   `cargo test -j 5 -p cratonvm-vm --lib jni_switches_move_as_one_package_and_refuse_arm_c`,
>   `cargo test -j 5 -p cratonvm-vm --lib the_in_native_pair_counts_once_and_waits_out_a_pause_on_the_way_out`;
>   regressions: the five d3/k tests below, `a_leaf_jni_function_runs_in_a_window_without_leaving_native`,
>   `native::jni::tests`, `threading::gc_barrier::tests`.
> - **Remaining gate:** item 4's `noop` bar (owners: `vm_exec.rs`'s deposit and
>   the take-over lanes, per the d10/j proposal); items 1 on G1 / ZGC; item 2
>   (the whole `cratonvm-vm --lib` with `CRATONVM_JNI_NATIVE_TRANSITIONS=1`
>   exported); item 3 (the real-workload JNI census and flipped runs).
>
> *Previous (d8/x, wave d7, Generational): correctness rows pass
> (`jniblock_D_1..3`, `jniblock_E_audit_1..3`, `jniroots_E_1..3`,
> `jniroots_E_audit_1..3`); cost A `noop 526 / array-length 17 / int-region 25 / new-string 502`,
> B `535 / 17 / 26 / 493`, E `4797 / 25 / 34 / 9097` ns/call.*

> **STATUS (2026-09-28, gcd d5/f): FIXED BEHIND OPT-IN FLAGS (unchanged: no
> default changed); the per-JNIEnv-call cost that blocked the flip is cut on
> the hot leaf functions, and the call's leave no longer re-deposits when
> nothing happened.** Written by reading; no cargo in the lane.
>
> *Where the ~8 us per JNIEnv call went* (`Gcd1JniCostProbe` arm E on the d4
> build: `array-length 7880 ns` against the default arm's `14 ns`): every
> JNIEnv call of a native in native made the full native -> VM -> native round
> trip, i.e. TWO full root-snapshot deposits (`check_post_block_gc`'s refresh
> on the way out, `deposit_root_snapshot` on the way back in: interpreter
> frames, class owners, deopt stash, off-frame roots, JNI local frames, the
> conservative JIT scan and pin publication, the frame-trace capture, the JMX
> monitor list, the reclaimed-slot audit), four barrier-lock round trips, the
> JIT blocked-band scan (`note_blocking_transition_enter`), the fixup and
> `slot_origins` locks and a TLAB retire. Not an OS-tid publish and not a
> take-over signal: those are outside the per-call path.
>
> *What landed (all inert unless `CRATONVM_JNI_NATIVE_TRANSITIONS` and
> `CRATONVM_JNI_INDIRECT_LOCALS` are on; byte-identical otherwise):*
>
> 1. **Leaf windows** -- HotSpot's `_thread_in_vm` for the functions that need
>    no more: `vm/src/threading/gc_barrier.rs` `LeafWindowWord`,
>    `GcBarrier::try_open_leaf_window`, `GcBarrier::drain_leaf_windows`
>    (called by the one pause-request core, `request_stw_counted_locked`,
>    right after `raise_stw_requested`); `vm/src/native/jni.rs`
>    `ForeignJniEntry::enter_leaf` / `enter_leaf_window`, `escalate_leaf_window`
>    (from `with_jni_context`), `NativeSync`. A leaf function (`GetArrayLength`,
>    `Get/Set<Boolean..Double>ArrayRegion` except `SetLongArrayRegion`,
>    `GetStringLength`, `GetStringUTFLength`, `DeleteLocalRef`) called with one
>    of the thread's own indirect locals stays EXCLUDED from pauses (its
>    deposited snapshot stays its roots) and runs inside a window: store
>    `open`, SeqCst fence, load `stw_requested`; a winning pause request
>    raises `stw_requested`, fences, and waits for every registered window to
>    close before it returns (Dekker: one side always sees the other). It also
>    checks that no pause completed and no relocation was folded into the
>    thread since the native went into native (`NativeSync`: the barrier's
>    `gc_generation` and the new per-thread `GcBlockState::blocked_folds`,
>    bumped by `ThreadRegistry::fold_pointer_map_into_blocked_audited`);
>    otherwise, or for a global ref / `jclass` / a pause in progress, it takes
>    the full transition as before. A leaf that must raise (an out-of-range
>    region) escalates to the full transition before building the exception.
>    Expected: `array-length` and `int-region` within ~2-5x of the default arm
>    (two fences and a few thread-local reads per call), i.e. inside the
>    brief's 10x target -- to be measured.
> 2. **Quiet leave** (`jni.rs::native_call_leave_native`, new
>    `GcBarrier::leave_blocked_region_flagged_if`): when the barrier finds
>    `NativeSync` unchanged at the moment it clears the flag (under its lock,
>    no pause possible), the leave skips `check_post_block_gc`: nothing was
>    folded, so there is no fixup, no native-slot capture, every `slot_origins`
>    entry still reads its origin, and the entry deposit still describes the
>    frames. Only a class redefinition in the window still converts frames.
>    Halves the in-native bracket of every native call (`noop`) and of every
>    non-leaf JNIEnv call. What remains per bracket is ONE full deposit on the
>    way in (lane u's `deposit_root_snapshot_inner`; see
>    `gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`).
>
> *Flip gate update* (the d4/k items 1-4 below stand; item 4 re-measured on
> a build with this change): `Gcd1JniCostProbe` arms A/B/E interleaved, 5
> runs, medians of medians. Expected on E: `array-length` and `int-region`
> <= 10x arm A (target of the gcd d5 brief); `noop` and `new-string` still
> dominated by the one entry deposit per bracket (expected roughly
> `noop` ~3 us, `new-string` ~3-4 us: one full re-entry deposit per
> `NewStringUTF`; `GetStringUTFLength` and `DeleteLocalRef` are leaves now).
> If `noop` misses the 2x-of-B bar, the flip waits for the proposal above,
> not for correctness evidence. Unit tests (new):
> `cargo test -j 5 -p cratonvm-vm --lib a_leaf_jni_function_runs_in_a_window_without_leaving_native`,
> `cargo test -j 5 -p cratonvm-vm --lib a_leaf_window_opens_outside_pauses_and_a_request_waits_for_it`,
> `cargo test -j 5 -p cratonvm-vm --lib leave_blocked_region_flagged_if_keeps_the_flag_up_when_told_to`;
> regression: the five d3/k tests, `native::jni::tests`,
> `threading::gc_barrier::tests`. Probe arms D/E of both d3/d4 probes must
> still print HotSpot's lines 3/3 (the quiet leave and the leaf windows sit on
> exactly their paths).
>
> **Previous STATUS (2026-09-28, gcd d4/k): FIXED BEHIND OPT-IN FLAGS; the flip gate
> is now written out in full below. No default changed.** First Linux runs of
> the d3 build (`e2024e4ad`, Generational, run 1 of 3): `Gcd1JniBlockInNativeProbe`
> arm D printed HotSpot's four lines (rc 0, ~1 s); arms A, B, C `rc=1` in
> ~30 s, as designed (`block-in-native` / `poll-in-native` time out while the
> pause waits for the counted native); `Gcd1JniRootsProbe` arm E printed
> HotSpot's seven lines (rc 0). Runs 2-3 and G1/ZGC are pending.
>
> **Design re-audit for a default flip (gcd d4/k, by reading
> `vm/src/native/jni.rs`).** The four ways the brief named for the design to
> bite as a default, checked:
>
> 1. *A JNIEnv function that runs uncounted.* Every one of the 202 table
>    entries was checked (a script over `build_function_table`, then the
>    macro bodies by hand). Without a `ForeignJniEntry` of their own are only:
>    `GetVersion`, `GetJavaVM`, `ExceptionCheck` (thread-local reads),
>    `ReleaseStringUTFChars` / `ReleaseStringChars` (a C buffer and the per-VM
>    record table's lock, no heap), `GetStringCritical` /
>    `ReleaseStringCritical` (delegate to functions that have it), the 31
>    `*MethodV` / `NewObjectV` wrappers and their `...` trampolines
>    (`va_list_to_jvalues` copies handles, decodes none, then the `*MethodA`
>    twin opens the entry), `FatalError`, `jni_stub`, and the invocation
>    table. None touches the heap or decodes a handle. The macro-generated
>    array functions (`new_prim_array`, `get/release_array_elements`,
>    `get/set_array_region`) all open it. **No gap.**
> 2. *A local that escapes indirection.* Every producer goes through
>    `record_local_handle_indirect`, which counts a raw fallback. One more
>    raw hand-out was found and is now counted: `ExceptionOccurred` returning
>    the raw pending handle when `native_pending_return` is empty (only
>    reachable without a thread context, never inside a native, but counted
>    anyway). `NewObjectA`'s post-`<init>` re-read keeps the indirect handle
>    (its slot probe compares the raw table entry with an indirect handle and
>    misses, so it returns the handle itself, which follows the move).
>    `PopLocalFrame` reads an indirect result's slot BEFORE the pop and
>    re-records it in the parent (checked). **No gap.**
> 3. *A critical region open across a transition.* `GetPrimitiveArrayCritical`,
>    `Get<Type>ArrayElements` and `GetStringChars`/`GetStringCritical` hand
>    out detached copies written back through a global ref at release, so a
>    move between Get and Release is invisible to the native; the transition
>    around a JNIEnv call made inside a critical section (which JNI forbids,
>    and HotSpot tolerates) is harmless here. **No gap.**
> 4. *`PopLocalFrame`'s result*: see 2.
>
> Residuals that are NOT gaps of the transition but the flip must know:
> `JNI_OnLoad` (run from `System.load`, `vm_exec.rs`, lane o's region) runs
> counted -- a library whose `JNI_OnLoad` blocks still holds pauses (HotSpot
> runs it in native); the non-x86-64 `...` slot (`jni_varargs_unsupported`)
> allocates an `UnsatisfiedLinkError` without an entry (x86-64 is the only
> target the flip is for); and native code that keeps a local past its
> native's return, hands one to another thread, or smuggles one through a
> `jlong` (all undefined behaviour) sees NULL or another slot once locals are
> indirect -- what item (3) below exists to find.
>
> **The flip gate, exactly.** The three flags flip TOGETHER
> (`CRATONVM_JNI_INDIRECT_LOCALS`, `CRATONVM_JNI_NATIVE_TRANSITIONS`,
> `CRATONVM_JNI_FOREIGN_TRANSITIONS`: the native transitions are inert
> without indirect locals, and an attached thread's locals are indirect only
> with the foreign transitions), each keeping its `=0` kill switch. Before it:
>
> 1. **Probes, 3/3 per collector** (Generational, G1, ZGC), Linux:
>    `Gcd1JniBlockInNativeProbe` arm D (four HotSpot lines),
>    `Gcd1JniRootsProbe` arm E (seven HotSpot lines), and both probes with all
>    three flags AND `CRATONVM_XT_ROOT_SCAN_AUDIT=1` (no `ROSTER HOLE`, no new
>    stderr `FAIL`). Expected pass rate 3/3 on an idle host; a FAIL under
>    parallel load is re-run alone first.
> 2. **Unit tests** (`cargo test -j 5 -p cratonvm-vm --lib`): the d3/k five
>    below plus `the_unbuilt_throwable_sentinel_never_reaches_native_code` and
>    `for_each_live_local_ref_visits_the_live_locals`; and the whole
>    `cratonvm-vm --lib` run once with the three variables exported (the
>    indirection and transition tests then run on the flipped defaults).
> 3. **A real-workload JNI census, then the same workloads flipped.** In
>    this VM the JDK's own natives are Rust registry natives, not JNI: a JNI
>    native is reached only through a host library's `RegisterNatives` or a
>    `Java_*` symbol in a library `System.load`ed by the application. So the
>    census is per workload. Command: add
>    `RUST_LOG=cratonvm::jni_census=debug` (new in d4/k: one `debug` line per
>    JNI native dispatch, `vm_exec.rs` JNI region, both dispatch arms) and
>    count `grep -o 'class=[^ ]* method=[^ ]*' | sort | uniq -c`. Drivers on
>    the Linux host, in order of value:
>    * the Netty suite runner (`/data/cratonvm/apps/netty-suite-runner`), its
>      native-transport classes (`io.netty.channel.epoll.*` tests: the event
>      loop blocks in `epoll_wait` INSIDE a JNI native -- the exact shape of
>      this page's defect) and `ParameterizedSslHandlerTest` (netty-tcnative
>      BoringSSL, callbacks into Java from inside natives);
>    * the Tomcat suite runner (`/data/cratonvm/apps/tomcat-suite-runner`):
>      expected to show zero JNI natives, since `org/apache/tomcat/jni/*` is
>      refused at dispatch -- a non-zero census there is itself a finding;
>    * the Spring Boot sample (`/data/sbjar-flat`) and the loader fat jars:
>      expected zero; confirms the flip is invisible to pure-Java apps;
>    * JNA (`libjnidispatch`), lz4-java and zstd-jni fixtures when present in
>      `~/.m2` (JNA's callback threads are the attached-thread path).
>    Pass criterion per workload: the flipped run's test verdicts equal the
>    unflipped run's (same `@@RESULT` / suite counts), no new crash, hang or
>    `stale ObjectRef` report, and `CRATONVM_DBG_JNI_LOCALREF=1
>    CRATONVM_DBG=gcpart` on the UNflipped run reports nothing that the flip
>    would change for the worse (a report there is a raw local the flip fixes).
> 4. **The per-call cost, measured with `Gcd1JniCostProbe`** (new in d4/k:
>    `tools/bench/Gcd1JniCostProbe.java` + `tools/probes/jni/Gcd1JniCostProbe.c`;
>    four cases -- empty native, `GetArrayLength` loop, one-element
>    `GetIntArrayRegion` loop, `NewStringUTF`+`GetStringUTFLength`+
>    `DeleteLocalRef` loop -- stdout `<case>: ok` x4 and `PASS all 4` on every
>    arm and on HotSpot, medians in ns/call on stderr). Run arm B
>    (`CRATONVM_JNI_INDIRECT_LOCALS=1`, the cost of indirection alone against
>    arm A) and arm E (all three), interleaved A/B/E per run, 5 runs, compare
>    medians of medians. HotSpot reference (WSL, JDK 25, one run):
>    `array-length` 9 ns, `int-region` 12 ns, `new-string` 75-89 ns. What to
>    decide on: B/A is the cost of indirect locals (one TLS frame lookup per
>    decode; expected small); E/B is the cost of the transitions (two deposits
>    per JNIEnv call from a native in native; expected LARGE -- microseconds
>    -- which is why `../../internal/gc/gcd-d3k-proposal-signal-free-in-native-threads-REJECTED-20260928.md`
>    proposes an incremental re-deposit). Suggested bar: E within 2x of B on
>    `noop` and within 5x on the JNIEnv loops; if E misses it, the flip waits
>    for that proposal, not for more correctness evidence.
>
> **d4/k code changes on this page's path:** the `ExceptionOccurred` raw-handle
> escape count (above); the census line (`vm/src/vm/vm_exec.rs`, both JNI
> dispatch arms, `tracing::debug!` target `cratonvm::jni_census`, inert unless
> that target is enabled); `for_each_live_local_ref` /
> `local_refs_may_be_held_raw` (`jni.rs`, `pub(crate)`, read-only) for lane
> m's JNI-local-aware pin.
>
> **Previous STATUS (2026-09-27, gcd d3/k): FIXED BEHIND AN OPT-IN FLAG
> (`CRATONVM_JNI_NATIVE_TRANSITIONS`, token `CRATONVM_GC=jni-native-transitions`,
> default OFF), and ONLY together with `CRATONVM_JNI_INDIRECT_LOCALS`. With
> either flag off every path is byte-for-byte as before.** Written by reading;
> no cargo in the lane. The probe's HotSpot oracle was run (WSL, OpenJDK 25,
> `-XX:+UseSerialGC -Xmx64m`): four lines, 3/3, rc 0, 30 young pauses in a run.
>
> **What landed (all `vm/src/native/jni.rs`).**
>
> * `dispatch_jni_native` brackets the C call (`call_jni_marshalled`) in a
>   `JniNativeCall`. Entry (`native_call_enter_native`): `java_state` 0 (the
>   thread reads RUNNABLE, as HotSpot reports `_thread_in_native`), TLAB
>   retired, `deposit_root_snapshot` (interpreter frames, compiled-frame roots
>   and pins, native pins -- the dispatch's pinned args and a synchronized
>   native's monitor -- and the JNI local frames, receiver and arguments
>   included), `in_blocked_region` up, `mark_blocked_region_enter`, arrive
>   (`arrive_and_wait_auto`) if a pause raced in. Exit
>   (`native_call_leave_native`, BEFORE the result is decoded):
>   `mark_blocked_region_leave` + `check_post_block_gc`. Every pause in between
>   excludes the thread, exactly as for `Object.wait`.
> * Every JNIEnv function: `ForeignJniEntry::enter` (already the first statement
>   of the 128 heap-touching functions and the handle-decoding JVMTI ones)
>   first asks `enter_from_native`: a thread whose `JNI_NATIVE_CALL` record is in
>   native leaves it for the function (waits out a pause, applies the fixups,
>   the local-frame table included) and re-enters on drop. Nested functions and
>   up-calls are counted; a native that Java calls from an up-call brackets its
>   own C call and restores the enclosing record.
> * **Which of "non-moving" or "rewrite": REWRITE, through indirect locals.**
>   The collector rewrites the thread's local-frame TABLE, never the native's
>   own copies. With the default raw-address locals a moving collection in the
>   native window would leave every `jobject` the native holds naming
>   from-space (`common-w2c-jni-local-refs-are-raw-addresses.md`, widened from
>   "an up-call inside the native" to "any collection by any thread"), and a
>   non-moving window is not expressible on every backend (a Generational young
>   copy cannot honour a per-object pin, only refuse the whole cycle). So the
>   transition is taken only where every local the native can hold is indirect
>   (`locals_are_indirect_here`: `CRATONVM_JNI_INDIRECT_LOCALS`, and on a
>   foreign-attached thread `CRATONVM_JNI_FOREIGN_TRANSITIONS` too). A raw handle
>   can still escape with both on (no open frame, a slot past the encodable
>   range): it bumps the thread's `raw_local_escapes`; a call whose receiver or
>   argument escaped never goes into native, and one whose JNIEnv function hands
>   one out stays counted from that function on. Everything else a native can
>   hold is safe across a move: global / weak-global refs are table indices, a
>   `jclass` is a tagged id, the array / critical / string `Get*` calls hand out
>   detached copies written back through a global ref, direct-buffer memory is
>   off-heap.
> * Found in review and fixed, default ON (HotSpot parity, wrong-result fix, no
>   switch): `DetachCurrentThread` called from inside a JNI native of a VM thread
>   cleared the JNI context, so every later JNIEnv call of that native answered
>   NULL; it now returns `JNI_ERR` and detaches nothing
>   (`jni_native_frame_on_stack`), as HotSpot does for a thread with Java frames.
>
> **Still open (owner: whoever runs the JNI workloads on the Linux host), in
> order:** (1) the probe matrix below; (2) arm E of
> `tools/bench/Gcd1JniRootsProbe.java` (all three JNI flags) prints HotSpot's
> seven lines 3/3 per collector; (3) the strict JNI corpus, netty-tcnative
> `ParameterizedSslHandlerTest`, JNA and lz4/zstd-jni with
> `CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_NATIVE_TRANSITIONS=1`: no new
> FAIL, crash or hang against the same run with the indirect flag alone; (4) a
> JNI-heavy throughput number (both flags vs indirect alone): every JNIEnv call
> made from a native in native pays a leave (fixup application + re-deposit)
> and a re-entry (deposit, conservative JIT scan when compiled frames are
> below); HotSpot pays two stores and a fence. **Flip gate:** (1)-(3) clean and
> (4) acceptable, then flip `native_transitions_active`'s unset case to ON
> TOGETHER WITH `CRATONVM_JNI_INDIRECT_LOCALS` (it is inert without it), keeping
> `CRATONVM_JNI_NATIVE_TRANSITIONS=0` as the kill switch. A residual to know:
> the take-over signals every alive thread with an OS tid, blocked ones
> included, so a thread in native still sees a signal per pass (an `EINTR` from
> `nanosleep` / `poll` / `epoll_wait`), as it does today while counted; HotSpot
> sends none. See `../../internal/gc/gcd-d3k-proposal-signal-free-in-native-threads-REJECTED-20260928.md`.
>
> **Tests** (`vm/src/native/jni.rs`, `mod tests`):
>
> ```
> cargo test -j 5 -p cratonvm-vm --lib a_java_thread_in_a_jni_native_holds_no_pause_and_its_jni_functions_wait_one_out
> cargo test -j 5 -p cratonvm-vm --lib a_jni_native_call_stays_counted_wherever_a_raw_local_could_escape
> cargo test -j 5 -p cratonvm-vm --lib a_nested_jni_native_call_restores_the_enclosing_record
> cargo test -j 5 -p cratonvm-vm --lib host_native_is_a_no_op_inside_an_in_native_jni_call
> cargo test -j 5 -p cratonvm-vm --lib detach_from_inside_a_native_method_is_refused_and_keeps_the_context
> ```
>
> (`host_thread_enter_native` / `_leave_native`, libcratonvm's
> `cratonvm_thread_enter_native`, are no-ops inside an in-native call: the
> host marking would empty the snapshot the native call deposited. They also
> resolve the calling thread's own VM now, `calling_thread_vm`, not the newest
> one -- identical with one VM.)
>
> **The probe** (Linux): `tools/bench/Gcd1JniBlockInNativeProbe.java` +
> `tools/probes/jni/Gcd1JniBlockInNativeProbe.c`. A 256 MB allocator runs
> while the main thread waits inside a native: in C with no JNI call
> (`block-in-native`), holding a local across that wait (`local-across-block`),
> and polling a Java flag through JNI with a string per round
> (`poll-in-native`). Each native gives up after 15 s, so the probe ends on
> every VM.
>
> ```bash
> gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
>     -o /tmp/libgcd1jniblock.so tools/probes/jni/Gcd1JniBlockInNativeProbe.c
> javac -d /tmp/gcd1jniblock tools/bench/Gcd1JniBlockInNativeProbe.java
> $JDK/bin/java -XX:+UseSerialGC -Xmx64m -cp /tmp/gcd1jniblock Gcd1JniBlockInNativeProbe /tmp/libgcd1jniblock.so
> R="timeout 300 cratonvm --java-home $JDK -Xmx64m -cp /tmp/gcd1jniblock"
> A=""                                                   # defaults
> B="CRATONVM_JNI_INDIRECT_LOCALS=1"
> C="CRATONVM_JNI_NATIVE_TRANSITIONS=1"
> D="CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_NATIVE_TRANSITIONS=1"
> for gc in UseGenerationalGC UseG1GC UseZGC; do for arm in A B C D; do for r in 1 2 3; do
>   env ${!arm} $R -XX:+$gc Gcd1JniBlockInNativeProbe /tmp/libgcd1jniblock.so \
>     >/tmp/blk-$gc-$arm-$r.out 2>/tmp/blk-$gc-$arm-$r.err; echo "$gc $arm $r rc=$?"
> done; done; done
> ```
>
> HotSpot and arm D print exactly (stderr carries `[gcd1-jni-block] ... waited
> N ms`, not compared):
>
> ```
> block-in-native: PASS
> local-across-block: PASS
> poll-in-native: PASS
> PASS all 3
> ```
>
> | arm | must print | evidence, not a verdict |
> |---|---|---|
> | A defaults | `local-across-block: PASS`; rc 0 or 1, never 124 | `block-in-native: FAIL code=1` (the defect: the allocator's first pause waits for the native, which times out at 15 s); `poll-in-native` may pass (a JNI allocation can reach a safepoint) or `FAIL code=1` |
> | B indirect only | as A | as A |
> | C native transitions only | as A (the flag is inert with raw locals: this arm is the check that it is) | as A |
> | D both | the four lines above, 3/3 on each collector; `block-in-native waited` far below 15000 ms | -- |
>
> A FAIL in D, or a crash, or rc 124 in any arm, voids the flip. Retire this
> page when the flag has flipped with (1)-(4) clean.
>
> **Previous state (2026-09-27, gcd d2/i): OPEN, as filed below.**

*Filed 2026-09-27 by gcd d2/i (lane threads), by reading; no build in the
lane.*

- **Status:** OPEN (as filed; see the STATUS block above).
- **Severity:** HIGH for liveness (a whole-VM hang for as long as the native
  blocks), all three collectors. No memory-safety consequence: the pause
  waits, nothing moves under the native.
- **Owner:** `vm/src/native/jni.rs` (JNI dispatch and the JNIEnv functions)
  with `vm/src/vm/vm_exec.rs` (the call site of `dispatch_jni_native`).

## What is wrong

HotSpot runs a JNI native method in `_thread_in_native`: the thread is
safepoint-SAFE for the whole native call, a pause does not wait for it, and
every JNIEnv function the native calls transitions native -> VM (blocking
for a pause in progress) and back. A native may therefore block in C for as
long as it likes -- `pthread_join`, `read`, `poll`, `sleep`, a JNA
`Native.invoke*` of any blocking C function -- and the rest of the VM keeps
collecting.

CratonVM does not model that state for a Java thread. The JNI call site in
`vm_exec.rs` (the `JniContextGuard::install_for_native` /
`JniImplicitFrameGuard::enter` block that ends in
`crate::native::jni::dispatch_jni_native`) makes no blocked-region
transition, so the thread stays a COUNTED mutator for the whole native call.
If any other thread requests a stop-the-world pause while the native is
blocked in C:

1. the census counts the thread (`alive_count_blocked_and_os_tids_for`: alive,
   `stw_ready`, not `in_blocked_region`);
2. the take-over signals it (Linux) or suspends it (Windows), finds `Rip`
   outside compiled code, and leaves it to "arrive cooperatively"
   (`STATE_NOT_JIT` / `Take::NotJit`);
3. it cannot arrive: it is in C. `stw_take_over_and_wait` loops on
   `wait_for_all_timeout` until the native returns, printing
   `STW cross-thread JIT takeover is still waiting for cooperative mutators`
   after 64 rounds.

Every mutator that reaches a safepoint meanwhile parks, so the whole VM
stops until the C call returns -- forever, if the C call is waiting for
something the parked Java threads would have done (a `pthread_join` on a
thread whose next up-call allocates; a `read` on a pipe a Java thread
writes). The foreign-attach design already solved this for HOST threads
(`host_thread_enter_native`, "the host-facing analog of HotSpot's
`_thread_in_native`"; `ForeignJniEntry` per function under
`CRATONVM_JNI_FOREIGN_TRANSITIONS`), but not for a Java thread that calls a
native.

Evidence by reading:

- `rg -n "enter_blocked|deposit_root_snapshot|mark_blocked_region" vm/src/native/jni.rs`
  finds none on the `dispatch_jni_native` path; the call site in
  `vm_exec.rs` (search `dispatch_jni_native(`) installs only the JNI context
  and the implicit local frame.
- `tools/bench/Gcd1JniRootsProbe.java` (this wave) had to be written around
  it: its native half never blocks inside a native while a collection can be
  pending (`foreignStart` returns at once, the Java side polls
  `foreignDetached` before `foreignJoin`), because the obvious shape --
  `pthread_join` inside the native that started an attached thread whose
  up-calls collect -- would hang CratonVM and not HotSpot.

Candidates in the wild: `docs/known-issues/netty/parameterizedsslhandlertest-residual-stalls-20260824.md`
(netty-tcnative natives), any JNA call of a blocking C function, JDBC
drivers with native network layers.

## Proposed fix

The two halves HotSpot has, reusing what the foreign-attach work built (see
the proposal `../../internal/gc/gcd-d2i-proposal-one-in-native-state-for-jni-REJECTED-20260928.md`):

1. Around `dispatch_jni_native` for a JNI (not VM-builtin) native: deposit
   the root snapshot (the interpreter frames and the JNI local frame are
   already the natives' roots) and enter the blocked region
   (`deposit_root_snapshot` + `GcBarrier::enter_blocked` /
   `mark_blocked_region_enter`, arriving if `pre_stw`), and leave it with
   `mark_blocked_region_leave` + `check_post_block_gc` on return.
2. Every JNIEnv function the native then calls must transition back to a
   counted mutator first -- exactly `ForeignJniEntry`, generalised from "a
   foreign thread at depth 0" to "any thread whose innermost frame is an
   in-native JNI call". Up-calls (`Call*Method*`) already nest through
   `ForeignCallGuard`'s depth logic.

With (1) and not (2) the natives would allocate and store while the thread
is excluded -- the w9g hazard, for every JNI native -- so the two land
together, behind an opt-in flag, and the default flips only after the JNI
corpus, netty-tcnative and JNA runs.

## How to verify

A probe whose native blocks in C while another Java thread allocates:
`static native void blockInC(int ms)` doing `usleep(ms * 1000)`, called
with 2000 ms while a second thread allocates 256 MB at `-Xmx64m`. HotSpot:
the allocator finishes during the sleep (print its elapsed time, expect
well under 2 s from its start). CratonVM today: the allocator's first pause
waits for the sleeping native, so it finishes only after the sleep. Retire
when the allocator finishes during the sleep on all three collectors.
