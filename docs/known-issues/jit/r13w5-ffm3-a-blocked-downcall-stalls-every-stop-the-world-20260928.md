# A thread blocked inside an FFM downcall stalls every stop-the-world pause

> **STATUS (2026-09-28, gcd d5/f): NARROWED; the transition stays OPT-IN
> (`CRATONVM_FFM_DOWNCALL_GC_SAFE=1`, round 13's stopgap flag, kept and now
> the flag of this fix -- no new flag). Default path unchanged except one
> wrong-result fix (errno capture, below).** Written by reading; no cargo in
> the lane.
>
> *What landed:*
>
> 1. **JNIEnv from C inside a GC-safe downcall** (a hazard of the stopgap: the
>    function ran while the thread was excluded from every pause). The vm's
>    `native::jni::ForeignJniEntry::enter` now leaves the downcall's region
>    first (`panama_libffi::leave_active_downcall_region`, through the
>    downcall's own `NativeContext`) and goes back on return
>    (`reenter_active_downcall_region`).
> 2. **Upcalls mark the region left while their Java runs**
>    (`panama_libffi::leave_downcall_region` / `reenter_downcall_region`,
>    called by `panama_upcall::upcall_entry` instead of the bare
>    `end/begin_blocking_region`): `ACTIVE_GC_SAFE` used to stay set during
>    the upcall, so a nested leave (item 1) would have ended the region twice.
>    **Merge point with JIT round 13 lane ffm4**: the two changed lines in
>    `upcall_entry`'s `with_active_context` closure (ffm4 changed that
>    function's no-downcall arm, ~8 lines below).
> 3. **`Linker.Option.critical` stays counted**, as on HotSpot (no
>    thread-state transition): new handle slot `panama::DOWNCALL_CRITICAL`
>    (`downcall_option_is_critical`, `downcall_set_critical`), set by the
>    address-less `downcallHandle` (`pe_downcall_handle_unbound`) and the
>    synthetic option path; `pe_downcall_invoke` decides
>    `gc_safe = flag && !critical` while the handle is fresh. The shipping
>    two-address form lives in `phases_late/foreign_ffm.rs` (JIT ffm4's
>    file): cross-lane request below, without it a critical handle is simply
>    treated as non-critical (slower under the flag, not wrong).
> 4. **Default ON (wrong results):** the capture state is read the moment
>    `ffi_call` returns (`panama::CapturedCallState::now`,
>    `write_downcall_capture_state`), no longer after the pin re-reads,
>    session releases, heap copy-back and `free`s that may overwrite `errno`;
>    and on Windows it is written at all -- `GetLastError`,
>    `WSAGetLastError`, `errno` at HotSpot's `captureStateLayout()` offsets
>    0/4/8, each only inside the segment's declared size -- where before
>    nothing was written and `captureCallState("GetLastError")` always read 0.
>    Probe line `captured call state: ok` (Linux `strtol` overflow -> `ERANGE`,
>    Windows `SetLastError(1234)`). No `=0` switch (HotSpot's stub saves the
>    state straight after the call).
>
> *Why still opt-in:* the region is `begin_blocking_region` /
> `end_blocking_region`, i.e. a full root-snapshot deposit in and a full
> wake (fixups + a second deposit) out per downcall, microseconds against
> HotSpot's ~10-17 ns `llabs` (HotSpot numbers from the new probe, Windows,
> JDK 25: `llabs` 17 ns, critical 10 ns), and a thread in it reports
> `WAITING`, not `RUNNABLE`. The cheap, `RUNNABLE` form is the JNI in-native
> machinery (`native::jni` `native_call_enter_native` + the quiet leave of
> gcd d5/f), which native-builtins cannot reach: it needs a `NativeContext`
> method -- cross-lane request (orchestrator; `native-api` is no lane's this
> wave) written out on
> `docs/known-issues/gc/gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`
> section 3. HotSpot runs every non-critical downcall in native, so the
> flip is right once the cost is.
>
> *Flip gate for `CRATONVM_FFM_DOWNCALL_GC_SAFE`:* (1) the probe below 3/3
> per collector (Generational, G1, ZGC) on Linux and Windows; (2) the cost
> A/B from the same probe's stderr (`[gcd5-ffm-cost] llabs median=...`),
> flag off vs on, 5 interleaved runs, on vs off within 1.5x (expected to FAIL
> today -- the deposit -- and to pass once the proposal's section 3 lands);
> (3) the foreign_ffm critical hunk applied, so `llabs-critical` stays at
> the flag-off cost; (4) the FFM probes of JIT rounds 12-13 unchanged with the
> flag on.
>
> *Probe:* `tools/bench/Gcd5FfmDowncallBlocksGcProbe.java` (adapted from
> `R13Ffm3DowncallBlocksGc`; adds a critical handle and a `qsort` whose Java
> comparator allocates and runs `System.gc()` from inside the upcall; ends on
> its own). HotSpot (`-XX:+UseSerialGC --enable-native-access=ALL-UNNAMED`,
> Windows and WSL Linux, JDK 25, checked 2/2 each): `warm sum 199990000`,
> `critical sum 199990000`, `qsort with allocating upcalls: sorted`,
> `upcalls survived a collection: true`, `captured call state: ok`,
> `gc finished while the worker was in C: true`, `worker returned 0`, `PASS`.
> CratonVM default arm: the same except `gc finished while the worker was in
> C: false` and `FAIL stall` (the defect); `CRATONVM_FFM_DOWNCALL_GC_SAFE=1`
> arm: HotSpot's eight lines.
>
> *Composition with JIT ffm4's cross-VM upcall wait*
> (`r13w8-ffm4-cross-vm-upcall-wait-gc-safe-patch-FIXED-20260929.md`, applied by the
> orchestrator after merging their wave 8): a thread in a GC-safe downcall
> that runs an upcall has LEFT the region (item 2, `ACTIVE_GC_SAFE` false,
> a counted mutator) before `run_upcall` hands off to VM B's helper thread;
> the patch's `begin/end_blocking_region` around the join is then an ordinary
> blocking region of a counted thread, nested inside nothing, and the upcall's
> `reenter_downcall_region` runs after it ends. A JNIEnv call C makes during
> that wait is not possible (the thread is in the join).
>
> *Cross-lane request (JIT ffm4 / orchestrator, `phases_late/foreign_ffm.rs`,
> the `Linker.downcallHandle(MemorySegment, FunctionDescriptor,
> Linker.Option[])` row; not a build dependency):*
>
> ```diff
>              let mut variadic_fixed: i64 = -1;
>              let mut capture_call_state = false;
> +            let mut critical = false;
>              if let Some(Value::Object(Some(opts))) = args.get(3) {
> ...
>                          if crate::panama::downcall_option_captures_call_state(ctx, opt) {
>                              capture_call_state = true;
>                          }
> -                        let kind = match ctx.get_field(opt, 0) {
> -                            Value::Int(k) => k,
> -                            _ => -1,
> -                        };
> -                        if kind == 0 {
> -                            variadic_fixed = match ctx.get_field(opt, 1) {
> -                                Value::Long(v) => v,
> -                                Value::Int(v) => v as i64,
> -                                _ => -1,
> -                            };
> -                        }
> +                        if crate::panama::downcall_option_is_critical(ctx, opt) {
> +                            critical = true;
> +                        }
> +                        if let Some(index) = crate::panama::downcall_option_first_variadic(ctx, opt) {
> +                            variadic_fixed = index;
> +                        }
> ...
>              let dh = crate::panama::alloc_downcall_handle(
> ...
>              )?;
> +            if critical {
> +                crate::panama::downcall_set_critical(ctx, dh);
> +            }
>              Ok(Some(Value::Object(Some(dh))))
> ```
>
> (The `firstVariadicArg` half fixes
> `docs/internal/gc/gcd-d5f-real-jdk-first-variadic-arg-option-is-ignored-FIXED-20260928.md`.)

Status: OPEN
Area: `native-builtins/src/panama.rs` (`pe_downcall_invoke`), `native-builtins/src/panama_upcall.rs`
(`upcall_entry`), `native-builtins/src/panama_libffi.rs` (`ActiveContextGuard`),
`vm/src/runtime/interpreter/gc_and_alloc.rs` (`stw_take_over_and_wait`), `vm/src/jit/xt_root_scan.rs`
(`take_over_pass`)
Severity: HIGH (liveness: a pause waits for as long as the C function runs; a C function that
waits for a Java thread that is itself waiting at the pause is a deadlock)
Found by: round 13 wave 5 lane ffm3 (by reading)

## What is wrong

HotSpot runs a downcall in `_thread_in_native`: a safepoint does not wait for it (only a
`Linker.Option.critical` call stays in Java state). CratonVM's downcall does not leave the
mutator state at all:

* `panama.rs` `pe_downcall_invoke` wraps `libffi::raw::ffi_call` in an `ActiveContextGuard`
  (so an upcall can re-enter Java) and in session acquires and pins, but never calls
  `begin_blocking_region` / `end_blocking_region`. The thread stays counted in every pause's
  `expected` (`ThreadRegistry::alive_count_blocked_and_os_tids` excludes only threads with
  `in_blocked_region`).
* `gc_and_alloc.rs` `stw_take_over_and_wait` (~883-1030) loops "until the barrier is satisfied";
  it only warns after 64 rounds. The take-over (`xt_root_scan.rs` `take_over_pass`) freezes a peer
  only when its `Rip` is in a registered JIT code range (`Take::Kept`); a thread in `read`,
  `poll`, `sleep`, `pthread_cond_wait`, `WaitForSingleObject` is `Take::NotJit` and is waited for.

So every stop-the-world pause (allocation-triggered, `System.gc()`, the frame-trace pause behind
`Thread.getStackTrace`) waits until each thread in a downcall returns. Deadlock shape: thread A
downcalls a C function that blocks on a condition thread B signals; B allocates first, its
allocation starts a pause, the pause waits for A.

## Reproducer

`C:\craton\jitr13-probes\src\R13Ffm3DowncallBlocksGc.java`: a worker sleeps in C
(`sleep(2)` / `Sleep(1500)`), main runs `System.gc()` while it is there and prints whether the
pause finished before the worker returned. HotSpot: `true`. Expected on CratonVM today: `false`
(the pause takes the whole sleep).

## Proposed fix

Two shapes; (b) is the right one, (a) is small enough to ship behind a default-off switch first.

(a) **Blocking region around the call** (`panama.rs`, lane-ffm-owned files only):
`ctx.begin_blocking_region()` right before `ffi_call`, `ctx.end_blocking_region()` right after;
on Linux read `errno` immediately after the call and put it back after the region ends (the
barrier's futex calls can overwrite it before `write_downcall_capture_state` reads it). An upcall
during the call must leave the region before it touches the heap: store a `gc_safe` flag next to
the context pointer in `panama_libffi`'s `ActiveContextGuard` slot, and in `upcall_entry` call
`end_blocking_region()` before `run_upcall` and `begin_blocking_region()` after it. Everything
`pe_downcall_invoke` holds across the call is already pinned and re-read (gc-common w20-f did it
for upcalls, which can collect too), so the region adds no new stale reference. Cost:
`begin_blocking_region` deposits a fresh root snapshot (a conservative JIT-frame scan, the frame
class owners, a SATB flush, a TLAB retire): tens of microseconds on a deep stack, against a few
nanoseconds for `strlen`. That is why it cannot be the default.

(b) **A cheap native state the take-over honours** (GC round, `xt_root_scan.rs` +
`gc_and_alloc.rs`, plus two stores in `pe_downcall_invoke`): a per-thread `in_downcall` word set
before `ffi_call` and cleared after; `take_over_pass` treats a peer with `in_downcall` set like a
peer in JIT code -- freeze it, scan its stack conservatively (the C frames hold no Java
references: every object the downcall uses is pinned), excuse it from the barrier -- and resumes
it after the pause. An upcall clears `in_downcall` and polls before running Java (it is a
mutator again), and sets it back on return. This is HotSpot's `_thread_in_native` at the cost of
two stores per call.

## How to confirm

* `R13Ffm3DowncallBlocksGc` prints `gc finished while the worker was in C: true`.
* `rg -n "begin_blocking_region|in_downcall" native-builtins/src/panama.rs` finds the transition in
  `pe_downcall_invoke`.
* Open question for the same fix (not verified; outside this lane's files): no blocking region
  was found around `vm_exec.rs`'s `dispatch_jni_native` call (~36659) either. If JNI natives
  stall pauses the same way, the fix belongs one level lower (the native-call funnel), not in the
  FFM path.

## Round 14 wave 1 (lane ffm): the default flip, decided by reading

### Is `CRATONVM_FFM_DOWNCALL_GC_SAFE` sound to turn on by default? Memory-safe: yes

The only downcall path is `panama.rs` `pe_downcall_invoke`: `lang_invoke` `mh_dispatch_body` and
the `lib.rs` door both call it, and there is no other `ffi_call`. Checked for the flag-on arm:

* **References held across the C call.** After `ffi_call` the body uses only the return layout,
  the return allocator, the capture-state segment, the permissive heap-argument segments and the
  acquired sessions. Each is pinned before the call and re-read through its pin after it
  (gc-common w20-f and round 12 wave 6). `handle` and `descriptor` are last used in
  `pe_downcall_prepare`, before the call. `args` / `call_args` are not read after it. The one
  exception is the `dbg_mh_dispatch` diagnostic, which reads `call_args.last()`'s field 0 after
  the call from an unpinned reference. That is diagnostic-only and was already stale under an
  upcall that collects. The caller's own frames are covered by the region's root deposit and
  fixup, as for every blocking native. A collection during the call is not new either: an upcall
  can already collect there, and every hazard it raised was fixed for that case.
* **Heap memory never reaches C.** Real JDK heap segments and byte/short/char alias segments are
  refused (`marshal_arg`, `Heap segment not allowed`). The int/long/float/double carve-out copies
  into a scratch NATIVE buffer before the call and back after it, through the re-read segment. So
  a moving collection during the call cannot move memory C is using. Native segments are not
  GC-managed; their sessions are acquired for the call.
* **Critical downcalls stay counted.** `gc_safe = flag && !downcall_is_critical(handle)`.
  `downcall_set_critical` is called by all three handle factories (`panama.rs` ~7054 and ~7239,
  `foreign_ffm.rs` ~7703; the d5/f cross-lane hunk is applied). This matches HotSpot's Java-state
  critical call, and it is the only road for `critical(true)` heap access.
* **Upcalls.** `upcall_entry` leaves the region (`leave_downcall_region`, which marks
  `ACTIVE_GC_SAFE` false) before `run_upcall` and re-enters after. So the upcall's Java,
  `upcall_fatal`'s uncaught-exception report and the exit all run as a counted mutator. A nested
  downcall installs its own guard, whose drop restores `prev_gc_safe`.
* **JNIEnv from C.** `ForeignJniEntry` leaves and re-enters through
  `leave_active_downcall_region` / `reenter_active_downcall_region` (gcd d5/f).
* **Exceptions.** `ffi_call` cannot raise a Java exception. Every refusal path returns before the
  region is entered: `pe_downcall_prepare`, the session acquire, the scratch allocation. A failure
  after the call (the address-return alignment check, the group copy) runs after
  `end_blocking_region`.
* **The cross-VM wait (this wave).** It runs inside `run_upcall`, after `upcall_entry` has left
  the downcall's region, so its `begin/end_blocking_region` pair nests inside nothing.
  `another_vms_stub_inside_a_downcall_runs_on_a_helper_of_that_vm` pins (1, 1) on a context with
  no downcall region open. Under the flag, a real VM sees A's thread: in native (region) -> left
  (upcall) -> blocked (cross-VM wait) -> left -> in native again -> left after `ffi_call`.
* **Thread exit inside C** (`pthread_exit` / `ExitThread` from the callee) never ends the region.
  That is undefined behaviour on HotSpot too, and was left alone.

### Why it is still not flipped here

Two things other than soundness stand in the way. Neither can be fixed inside the FFM files:

1. **`Thread.getState()` would change in `--compatible` too.** `begin_blocking_region` reports
   `WAITING`. HotSpot, and CratonVM with the flag off, report `RUNNABLE` for a thread in a
   downcall. The fix is a `NativeContext::begin_native_region` (java_state 0). It is written out
   as an exact patch, with the FFM half and the flag-row change, on
   `r14w1-ffm-downcall-in-native-state-patch-20260929.md`.
2. **The cost gate (d5/f gate item 2) is unmet.** The region is still a TLAB retire, a full root
   deposit and a full wake per downcall. The light in-native deposit (the gcd-d5f proposal,
   section 3) has not landed; the patch page says how the JNI bracket's quiet leave could serve
   FFM. Flipping now would turn every small downcall (`strlen`, the Elasticsearch vector
   kernels) from tens of nanoseconds into microseconds.

When both are done, the flip is the one-line reader change on the patch page
(`runtime_flag_on` to `runtime_flag_default_on`, flag row `off_word: Some("0")`).
`Linker.Option.critical` handles keep the old state through `downcall_is_critical`.

Probe: `C:\craton\jitr14-probes\src\R14FfmBlockedDowncallPause.java` (counter-based:
`gc-progress ok` when at least 3 of 6 allocate-and-`System.gc()` rounds finish while a worker
sleeps 3 s in C). HotSpot: `gc-progress ok`. CratonVM default: `gc-progress stalled rounds=0`.
`CRATONVM_FFM_DOWNCALL_GC_SAFE=1`: `gc-progress ok`.

Status stays OPEN.

### Round 14 wave 1 (lane ffm), later the same wave

Steps 1-3 of `r14w1-ffm-downcall-in-native-state-patch-20260929.md` are applied: a GC-safe
downcall now reports RUNNABLE (`NativeContext::begin_native_region`), and the flag stays opt-in
for the orchestrator's cost A/B. The probe now uses `libraryLookup("kernel32.dll")`.
`resolve_library_path` passes a bare name through to `LoadLibrary` unchanged, and the explicit
name is also safe for the JDK's own `RawNativeLibraries` road.

## Round 14 orchestrator: the cost A/B (flip gate item 2) -- FAILS, flag stays opt-in

w5a binary (round 14 waves 1-5, Windows release), `Gcd5FfmDowncallBlocksGcProbe`,
`-XX:+UseGenerationalGC`, 5 interleaved runs per arm, `[gcd5-ffm-cost]` medians in ns/call:

| case | `CRATONVM_FFM_DOWNCALL_GC_SAFE` off | `=1` |
|---|---|---|
| `llabs` | 3937 / 4023 / 4125 / 4338 / 4013 (median 4023) | 17259 / 17295 / 13272 / 17688 / 11906 (median 17259) |
| `llabs-critical` | 3913 / 3951 / 4049 / 4588 / 4030 (median 4030) | 8823 / 4098 / 4264 / 4129 / 4110 (median 4129) |

On vs off is 4.3x for a plain downcall, against the gate's 1.5x. As the gate predicted, the
in-native deposit is the cost (`docs/known-issues/gc/gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`
section 3, GC-owned). Gate item 3 holds: the critical handle stays at the flag-off cost.
`R14FfmBlockedDowncallPause` still reads `gc-progress stalled rounds=0` in the default arm, which
is the expected default-off behaviour. The flip waits for the GC round's light deposit.
