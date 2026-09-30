# A foreign-attached thread publishes no OS tid, so no take-over roster and no helper-window pass can see it

> **STATUS (2026-09-29, gce e1/x): KEEP -- unchanged; gated by the package flip.** Audited package rows `*_jniroots_P3_1..3` (`CRATONVM_XT_ROOT_SCAN_AUDIT=1`) = HotSpot 3/3 per collector (`verify-e1/ve1`); the `ROSTER HOLE` count is not in the retained summaries. **Remaining:** that count (0 expected) from the rows' stderr, then retire with the package flip.

> **STATUS (2026-09-28, gcd d10/j, lane jni10; by reading, no cargo):
> unchanged in code -- FIXED BEHIND THE JNI IN-NATIVE PACKAGE (default OFF);
> retires with its flip.** The tid publish rides on
> `CRATONVM_JNI_FOREIGN_TRANSITIONS`, which is now switched with the package
> (`CRATONVM_JNI_NATIVE_TRANSITIONS=1` turns it on, `vm/src/native/jni.rs::jni_switches`)
> and refused without indirect locals (arm C). The publish and withdrawal
> (`foreign_leave_idle` / `foreign_enter_idle`) are untouched. Expected, each of
> `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`: `Gcd1JniRootsProbe`
> with `CRATONVM_JNI_NATIVE_TRANSITIONS=1 CRATONVM_XT_ROOT_SCAN_AUDIT=1 CRATONVM_DBG_XT_JIT_ROOT_SCAN=1`
> prints HotSpot's seven lines 3/3 with `grep -c 'ROSTER HOLE'` = 0 on stderr.
> Gate: the counted-mutators page's (d10/j STATUS).
>
> *Previous (d8/x, wave d7): unchanged -- FIXED BEHIND `CRATONVM_JNI_FOREIGN_TRANSITIONS` (default OFF); retires with that flag's flip.* The roster-audit rows pass: `Gcd1JniRootsProbe` arm E with `CRATONVM_XT_ROOT_SCAN_AUDIT=1 CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` (`jniroots_E_audit_1..3`) prints HotSpot's seven lines 3/3 with 0 `ROSTER HOLE` over 20 000+ `linux pass` lines per run, and `Gcd1JniBlockInNativeProbe` with all three flags and the audit (`jniblock_E_audit_1..3`) 3/3, 0 holes. Nothing else to measure here; the flip is the counted-mutators page's gate.

> **STATUS (2026-09-28, gcd d5/f): unchanged -- FIXED BEHIND
> `CRATONVM_JNI_FOREIGN_TRANSITIONS` (default OFF); retires with that flag's
> flip.** Reviewed: d5/f adds no OS-tid publish or withdrawal on any path (the
> leaf windows and the quiet leave run on a Java thread in a JNI native, whose
> tid is published by its own start); the ~8 us per-call cost of the native
> transitions was never this publish (it is two deposits per call; see the
> counted-mutators page's d5/f STATUS).

> **Previous STATUS (2026-09-27, gcd d3/k): FIXED BEHIND `CRATONVM_JNI_FOREIGN_TRANSITIONS`
> (default OFF; no new flag). With the flag off nothing changes.** Written by
> reading; no cargo in the lane.
>
> **What landed.** Not the "publish once at attach" the page proposed: an
> idle attachment is GC-blocked with no Java or compiled frame, and a
> published tid would put it in every take-over pass's signal list
> (`alive_count_and_os_tids` lists blocked threads too) and every
> helper-window pass's (`blocked_os_tids`), i.e. a signal (Linux) or a
> suspend (Windows) per pass per idle host thread -- an `EINTR` out of the
> host's `epoll_wait` for nothing. Instead the tid is published exactly while
> the attachment runs Java or a JNI function:
>
> * `vm/src/native/jni.rs::foreign_leave_idle` (every idle -> running
>   transition: `ForeignCallGuard`, `ForeignJniEntry`, the detach-time
>   `ThreadLocal` release) calls the new
>   `ThreadRegistry::republish_os_tid_current` after the barrier leave and
>   before `check_post_block_gc` clears the flag, so the first pause that
>   counts the thread also lists it;
> * `foreign_enter_idle` (running -> idle) calls the new
>   `ThreadRegistry::clear_os_tid` while the thread is still a counted mutator
>   in Rust code, before it raises the flag;
> * `vm/src/threading/thread_registry.rs`: `republish_os_tid_current` is
>   `set_os_tid_current` without its once-per-thread debug-canary
>   registration (that one leaks an `Arc` per call); `clear_os_tid` stores 0
>   and leaves the ordinal (a 0 tid matches no claimant query).
>
> So a callback thread running compiled code can be frozen and scanned by the
> take-over like any Java thread, and one that blocks inside a callback is in
> the helper-window roster. Side effect to know: `ThreadMXBean` CPU time of an
> attached thread (`os_tid_of`) answers only while it runs (it never answered
> before).
>
> **Test:** `cargo test -j 5 -p cratonvm-vm --lib an_attached_thread_publishes_its_os_tid_only_while_it_runs`
> (flag on: no tid while idle, a tid inside a `ForeignJniEntry` on Linux and
> Windows, none again after it; flag off: none inside a `ForeignCallGuard`).
>
> **Runtime check** (Linux, the page's "How to verify"; `Gcd1JniRootsProbe`
> now takes an optional second argument, the attached thread's iteration
> count, 2000 by default):
>
> ```bash
> CRATONVM_JNI_FOREIGN_TRANSITIONS=1 CRATONVM_XT_ROOT_SCAN_AUDIT=1 CRATONVM_DBG_XT_JIT_ROOT_SCAN=1 \
>   timeout 600 cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp /tmp/gcd1jni \
>   Gcd1JniRootsProbe /tmp/libgcd1jniroots.so 20000 2>/tmp/jni-roster.err
> grep -c 'ROSTER HOLE' /tmp/jni-roster.err    # expect 0
> ```
>
> stdout must still be the probe's arm-C lines (w9g page); HotSpot prints the
> seven PASS lines with the 20000 argument too (verified in WSL, OpenJDK 25).
> Retires with `CRATONVM_JNI_FOREIGN_TRANSITIONS`'s flip (its gate is on
> `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md`); until
> then it is FIXED-behind-flag.
>
> **Previous state (2026-09-27, gcd d2/i): OPEN, as filed below.**

*Filed 2026-09-27 by gcd d2/i (lane threads), by reading; no build in the
lane.*

- **Status:** OPEN (as filed; see the STATUS block above).
- **Severity:** MEDIUM. Liveness (a pause waits for an attached thread that
  runs compiled code until its next poll) and a false alarm in the roster
  audit; the coverage accounting keeps it from being a memory-safety bug
  (below).
- **Owner:** `vm/src/native/jni.rs` (`attach_foreign_thread`), with the
  take-over (`vm/src/jit/xt_root_scan.rs`) as the reader.

## What is wrong

Every thread the VM starts publishes its OS tid on its own thread before it
can run Java (`ThreadRegistry::set_os_tid_current`: main in `Vm::new`,
`Thread.start` carriers, native carriers). The take-over's roster
(`alive_count_and_os_tids`), the helper-window roster (`blocked_os_tids`,
which drops a `0` tid) and the newest-claimant routing of the blocked-peer
native-slot remap (`newest_claimants_by_os_tid`) are all keyed by it, and
the Windows `take_over_pass`'s coverage obligation says so: compiled code is
entered only through `JitEntryGuard`, only on Java threads, "all of which
are in the registry with a published OS tid".

`attach_foreign_thread` (`vm/src/native/jni.rs`) never calls
`set_os_tid_current`. An attached host thread that calls into Java
(`CallVoidMethod`, a JNA callback) runs interpreted and compiled code with
`os_tid == 0`:

- it is in no take-over roster, so a pause that finds it spinning in
  compiled code cannot freeze and scan it; the pause waits for its next
  safepoint poll (it is counted), which a poll-free loop may not reach soon;
- blocked inside such a call (a callback that waits on a lock), it is in no
  helper-window roster, so its compiled frames get neither the conservative
  pin nor the helper-window refusal. What keeps that from being unsound is
  the coverage accounting: its JIT depth is in `GLOBAL_JIT_DEPTH` and it
  deposits no proven depth, so the shortfall refuses the moving cycle; and
  its blocking deposit's conservative JIT scan pins (`publish_pinned_jit_roots`)
  cover G1/ZGC. So the gap is lost coverage machinery, not a lost root;
- with `CRATONVM_XT_ROOT_SCAN_AUDIT=1` on Linux, `/proc/self/task` includes
  it, so if it parks in compiled code the audit prints `ROSTER HOLE` -- the
  signature `common-c-linux-takeover-signals-every-thread-FIXED-20260929.md`'s soak treats
  as a take-over soundness failure. The soak's three probes attach no host
  thread; a JNI workload run with the audit would.

## Proposed fix

Publish it: in `attach_foreign_thread`, after `register_with_daemon` /
`register_starting_with_daemon` and the `set_*` wiring, call
`shared.threads.thread_registry.set_os_tid_current(tid)` (it runs on the
attaching OS thread, which is its contract), and clear nothing extra on
detach (`mark_dead` already takes the entry out of every roster). Opt-in
first: it adds the attached threads to every take-over pass (a signal or
suspend per pass per attached thread) and to the helper-window pass when
they block with compiled frames -- a cost and a behaviour change on the
workloads that attach many threads (netty-tcnative, JNA callback pools).

## How to verify

`tools/bench/Gcd1JniRootsProbe.java` on Linux with
`CRATONVM_XT_ROOT_SCAN_AUDIT=1 CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` and a
compiled callback (raise `FOREIGN_ITERS` so `accept` tiers up): today a
`ROSTER HOLE` line can appear for the attached thread's tid; with the fix,
none, and `linux pass: signaled N` counts it.
