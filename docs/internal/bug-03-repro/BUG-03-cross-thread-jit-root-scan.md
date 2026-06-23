# BUG-03 — Cross-thread STW JIT conservative root scan (gated mitigation)

**Branch:** `fix/bug-03-cross-thread-jit-roots` (worktree `C:/craton/CratonVM-xtjitroots`)
**Status:** implemented, **default-OFF** (`CRATONVM_XT_JIT_ROOT_SCAN=1`), bt16/bt18 regression-clean (safe scaffolding). **NOT a confirmed fix** — see "Decisive negative result" below: the OS-suspend + Rip-classification strategy does not engage on available synthetic repros and is shown to be *insufficient on its own* to close the gap (it cannot cover peers whose roots are held while their `Rip` is in a JIT helper). Kept as default-off scaffolding + an honest record of what doesn't work. Not pushed; not merged to dev.

## The bug

`org.springframework.aop.interceptor.ConcurrencyThrottleInterceptorTests` SIGSEGVs (`rc=139`) under `CRATONVM_GC_STRESS`. Root cause (per the original handoff): a thread executing JIT-compiled code never reaches an interpreter safepoint (JIT does not poll the STW flag), so when a *peer* thread stops the world, the in-JIT thread's live object roots — held only in its registers / JIT stack frames — are visible to the collector only via its last-published `root_snapshot`. If that snapshot is stale, the non-moving sweep reclaims a still-live object and the in-JIT thread later dereferences the freed slot → crash. The gap detector (`conservative_roots::warn_cross_thread_jit_gap`) fires 16-17×/run on this test.

## The fix (what this branch adds)

A cross-thread stop-the-world JIT conservative root scan, Windows-only, gated behind `CRATONVM_XT_JIT_ROOT_SCAN`.

When a multi-threaded GC initiator stops the world (`maybe_gc` / `maybe_gc_forced` / `force_gc_from_native`), before it marks it:

1. **Enumerates every other OS thread** of the process (`CreateToolhelp32Snapshot`).
2. **OS-suspends** each peer (`SuspendThread` + `GetThreadContext`).
3. **Classifies its `Rip`** against a *pre-suspend snapshot* of the registered JIT code ranges (`jit_code_ranges_snapshot`). If `Rip` is inside a compiled-code range, the thread is executing pure JIT instructions and holds **no** VM/Rust lock (every lock-taking helper is itself Rust code, so its `Rip` would be *outside* the JIT range) — it is safe to keep frozen across the collection.
   - **In-JIT** → conservatively scan its 16 integer registers + used stack (`[Rsp, committed-region-end)`, fault-bounded via `VirtualQuery`) for heap object addresses (validated by the lock-free `VmHeap::is_object_address`), add them as roots, keep it suspended, and exclude it from the barrier's `expected` count (`GcBarrier::reduce_expected`) so `wait_for_all` cannot hang on it.
   - **Not in JIT** (interpreter / native / parked) → resume immediately; it arrives cooperatively or is already blocked-excluded. Never kept frozen (it might hold a lock → deadlock).
4. **Publishes the frozen peers' un-retired TLAB tails** as sweep skip-regions (see below).
5. After `complete_gc`, **resumes** the frozen peers.

### Why it is sound

- A frozen in-JIT peer keeps its JIT-entry guard live, so `gc_quiescence::is_active()` stays true and the heap performs a **non-moving** sweep. Conservatively-discovered roots are never relocated — pinning is implicit; an interior / false-positive pointer can never be mis-rewritten.
- `is_object_address` is lock-free and inclusive: a false positive only over-retains; a real object is never missed.
- Only threads with `Rip` in pure compiled code (no lock) are held across mark/sweep, so the collector can never deadlock on a lock owned by a frozen thread.
- **Deadlock avoidance for classification:** the code-range registry (`JIT_CODE_RANGES`) is itself lock-guarded and that same lock is held by `register_jit_code_range` during compilation. Calling `lookup_jit_code_range` on an already-suspended peer that was mid-registration would deadlock. We therefore snapshot the ranges **before** suspending anyone and classify each frozen peer against the lock-free local copy.

### TLAB walkability (the subtle part)

A peer forcibly stopped mid-JIT never reached a safepoint to `retire` (tail-fill) its TLAB. Its un-filled tail `[cursor, end)` (garbage — the JIT does not reliably zero TLAB refills) would desync the non-moving sweep's linear heap walk and could get reclaimed → after resume the peer bump-allocates into freed memory → corruption.

The JIT writes the **full object header before committing the bump cursor** (verified in `x64.rs:emit_inline_tlab_new`; the cursor store is the single TSO-ordered linearization point), so at any frozen instant `[base, cursor)` is fully walker-coherent and `[cursor, end)` wholesale-covers any in-flight object. The fix reads each frozen peer's `[cursor, end)` (`Tlab::reserved_tail`, race-free because the peer is suspended) and passes them as **skip-regions** merged into the non-moving young sweep's free-block snapshots (`GenerationalHeap::sweep_young_non_moving`). The walk then treats the tail like an already-free block — neither walked as objects nor reclaimed — so the reservation survives the collection intact and the in-flight object (also pinned via the register scan) is safe. No peer memory is written.

Blocked / parked / cooperatively-arrived peers have already retired their TLABs (`reserved_tail` → `None`), so they contribute no skip region; the default path is byte-identical.

## Files changed

- `jit/src/lib.rs` — `jit_code_ranges_snapshot()`, `xt_jit_root_scan_enabled()`, and gate `register_jit_code_range` on `precise_jit_maps_enabled() || xt_jit_root_scan_enabled()` so the registry is populated when the feature is on.
- `vm/src/jit/xt_root_scan.rs` — **new**: OS-suspend + conservative register/stack scan, deadlock-safe Rip classification, `TakenOver` / `take_over_pass` / `resume`.
- `vm/src/jit/conservative_roots.rs` — `warn_cross_thread_jit_gap` early-returns when the feature is on (the gap is now covered at GC time → strict mode reaches 0 hits).
- `vm/src/threading/gc_barrier.rs` — `reduce_expected`, `wait_for_all_timeout`.
- `vm/src/threading/thread_registry.rs` — per-thread `tlab_addr` + `set_tlab_addr`/`clear_tlab_addr`/`collect_reserved_tlab_tails`.
- `vm/src/vm/vm_exec.rs`, `vm/src/native/jni.rs`, `vm-cli/src/main.rs` — publish each thread's TLAB address at start, clear on teardown (main / workers / foreign).
- `gc/src/tlab.rs` — `Tlab::reserved_tail()`.
- `gc/src/gen_heap.rs` — `jit_tlab_skip_regions` storage + `set/clear`, merged into the two production sweep free-block snapshots.
- `gc/src/vm_heap.rs` — `supports_jit_tlab_skip` (Generational only) + skip-region pass-through.
- `vm/src/runtime/interpreter.rs` — `stw_take_over_and_wait` driver wired into the three multi-threaded GC initiator paths.

## Validation performed

- `cargo check`/release build clean.
- **bt16 = 14985902, bt18 = 68332206** — both **MATCH HotSpot** with flag OFF *and* flag ON (binarytrees repro, `--Xmx 3g`). The non-take-over changes (code-range registration gate, the `conservative_roots:1009` A5 scan it activates, barrier methods, TLAB-skip plumbing) do not regress single-threaded GC correctness.
- Synthetic multithreaded repro (`XtJitRepro.java`): machinery reached (GC passes run), gate reads the flag (`jit_gate=true`), code ranges register, the take-over loop is storm-free, and the classification correctly declines to freeze threads whose `Rip` is in a helper.

## Decisive negative result (synthetic repro)

`XtJitRepro.java` (6 workers in a hot JIT `build()` loop, `--Xmx 24m` for frequent natural GC) **does reproduce a dropped-root corruption** (wrong checksum) ~1 in 6–10 runs — **at the same rate with the flag ON and OFF**, with **`took_over=0` in every run** (corrupt and clean alike). Correlation over 10 flag-on runs:

```
run 1..9 : RESULT OK         took_over=0 in_jit_passes=8 takeover_passes=0
run 10   : RESULT CORRUPTION  took_over=0 in_jit_passes=8 takeover_passes=0
```

So the corruption this repro hits is **not** the cross-thread-JIT-peer case the fix targets, and the take-over never engages. The deeper reason: although `any_thread_in_jit()` (`GLOBAL_JIT_DEPTH > 0`) is true on 8 GCs/run, those in-JIT threads' `Rip` is essentially never inside a *registered compiled-code range* at the stop-the-world instant — they are in JIT-called **helpers** (alloc slow path, dispatch), OSR code, or interpreter transitions. **`in-JIT-depth ≠ Rip-in-classifiable-compiled-code.`**

**Implication / limitation of this approach.** The fix can only *safely freeze* a peer whose `Rip` is in pure compiled code (no lock held). But the gap the detector measures (and likely the real crash) includes peers whose live roots are held while their `Rip` is in a helper — which cannot be frozen without deadlock risk and are not covered here. **The OS-suspend + Rip-classification strategy is therefore insufficient on its own to close BUG-03.** A complete fix almost certainly needs cooperative JIT safepoint polling (so an in-JIT thread reaches a known-safe point with a precise oop map), which is the larger deferred feature. The machinery in this branch (TLAB-skip plumbing, barrier exclusion, conservative frozen-frame scan) is reusable scaffolding toward that, but the classification gate is the limiting factor.

The ~1/6 corruption this repro DOES hit is a concrete, runnable reclamation bug (no Spring classpath needed) and is the recommended next thread to pull — it may be the same root cause as BUG-03 and as the startup-corruption finding below.

## What is NOT yet validated (honest gaps)

1. **A take-over EVENT firing / the crash being fixed.** Synthetic repros could not reliably create the exact "*peer* executing pure compiled code while a *different* thread stops the world" timing: in the tight-loop repro the in-JIT thread at GC time is almost always the *initiator itself* (correctly handled by self-scan), and OSR-only methods aren't always registered. The real Spring test does create it (16-17 gap hits/run) but its classpath is unavailable here. **Validation TODO:** run `CRATONVM_XT_JIT_ROOT_SCAN=1 CRATONVM_STRICT_JIT_ROOTS=1 CRATONVM_GC_STRESS=4096 KRun …ConcurrencyThrottleInterceptorTests` — expect 0 gap hits and no `rc=139`, with `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` showing take-overs engage.
2. **Coverage scope.** This covers peers in *pure compiled code* (the spinning-loop case the handoff identifies as the crash cause). A peer in a JIT-*called helper* (Rip in Rust) is intentionally NOT frozen (lock-deadlock risk); those are covered by the existing snapshot mechanism (object-returning natives publish), but not universally — so the fix may not drive *all* gap hits to zero on every workload.
3. **Default-on flip** needs the full app gauntlet (matches the project's gated-lever convention).

## Separate pre-existing finding (NOT caused by this fix)

Under aggressive `CRATONVM_DBG_GC_STRESS` (≤ ~256 KiB young threshold), the VM corrupts live `Thread` objects **during thread startup** and crashes with `NullPointerException: ... "this.holder" is null` in `Thread.<init>`. This reproduces **identically with the flag OFF** (baseline) — it is a pre-existing aggressive-GC-stress startup bug, independent of this change, and it masks high-GC-stress validation of this fix. Thresholds ≥ ~512 KiB are clean. Worth a separate investigation (it is a live-object-reclamation bug in the same family as BUG-03 but triggered single-threaded at startup).
