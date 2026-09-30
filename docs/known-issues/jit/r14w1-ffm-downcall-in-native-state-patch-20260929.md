# A GC-safe FFM downcall should report RUNNABLE, not WAITING (patch, then the flip)

Status: OPEN (round 14 wave 1: steps 1-3 APPLIED with the flag still opt-in; step 4 = the orchestrator's cost A/B, then the flip)
Area: `native-api/src/registry.rs` (`NativeThreadAccess`), `vm/src/vm/vm_exec.rs`
(`impl NativeThreadAccess for NativeContextImpl`), `native-builtins/src/panama.rs`
(`pe_downcall_invoke`, `ffm_downcall_gc_safe`), `native-builtins/src/panama_libffi.rs`
(`reenter_downcall_region`), `types/src/flag_groups.rs` (the `ffm-downcall-gc-safe` row)
Severity: HIGH as part of `r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world`
(liveness). This page covers only its `Thread.getState()` half.
Found by: round 14 wave 1 lane ffm (by reading, while deciding the default flip of
`CRATONVM_FFM_DOWNCALL_GC_SAFE`)

## What is wrong

`CRATONVM_FFM_DOWNCALL_GC_SAFE` runs the C call inside `begin_blocking_region` /
`end_blocking_region`. `NativeContextImpl::begin_blocking_region` is
`begin_blocking_region_with_state(1)`, so while the thread is in C, `Thread.getState()` answers
`WAITING` (`ThreadRegistry::java_block_state` reads `java_state` while `in_blocked_region` is
up). HotSpot answers `RUNNABLE` for a thread in native, and so does CratonVM today with the flag
off (a counted mutator). The same is true for the JNI in-native bracket (`native/jni.rs`
`native_call_enter_native` stores `java_state` 0). Flipping the flag default-on as it stands would
change `Thread.getState()`, thread dumps and `ThreadMXBean` for every thread in a downcall, in
`--compatible` too.

## Patch

### 1. `native-api/src/registry.rs`, `NativeThreadAccess`, after `begin_timed_blocking_region`

```rust
    /// The GC-safety contract of [`Self::begin_blocking_region`] for a thread
    /// that runs foreign code (an FFM downcall): a pause does not wait for it,
    /// but `Thread.getState()` stays `RUNNABLE`, as HotSpot reports a thread in
    /// `_thread_in_native`. Paired with exactly one `end_blocking_region`.
    ///
    /// The default delegates to `begin_blocking_region` (reported `WAITING`),
    /// so out-of-tree implementors need not change.
    fn begin_native_region(&mut self) {
        self.begin_blocking_region();
    }
```

### 2. `vm/src/vm/vm_exec.rs`, `impl<'a> NativeThreadAccess for NativeContextImpl<'a>`, after `begin_timed_blocking_region` (~20588)

```rust
    fn begin_native_region(&mut self) {
        // `java_state` 0: RUNNABLE while GC-safe, as the JNI in-native bracket
        // (`native::jni::native_call_enter_native`) reports it.
        self.begin_blocking_region_with_state(0);
    }
```

`java_state` 0 with `in_blocked_region` up is already the JNI bracket's combination, so no reader
meets a new state. `end_blocking_region` is unchanged: it does not read `java_state`.

### 3. The FFM half (lane ffm's files; apply after 1 and 2)

* `native-builtins/src/panama.rs` `pe_downcall_invoke`: in `if gc_safe { ctx.begin_blocking_region();
  ctx_guard.set_gc_safe(true); }`, change `begin_blocking_region` to `begin_native_region`.
* `native-builtins/src/panama_libffi.rs` `reenter_downcall_region`: change
  `ctx.begin_blocking_region();` to `ctx.begin_native_region();`. The upcall and the JNIEnv entry
  go back in through it.
* `native-builtins/src/panama.rs` `ffm_downcall_gc_safe`:
  `runtime_flag_on("CRATONVM_FFM_DOWNCALL_GC_SAFE")` becomes
  `runtime_flag_default_on("CRATONVM_FFM_DOWNCALL_GC_SAFE")`, and the doc line "default OFF"
  becomes "default ON (round 14); `=0` keeps the thread a counted mutator".
* `types/src/flag_groups.rs` row `ffm-downcall-gc-safe`: `off_word: Some("0")` (keep
  `off_key: None`, as the other default-on FFM rows), then regenerate the flag docs.

The mock (`native-builtins/src/test_utils.rs`) does not override the new method, so the
default's delegation keeps `blocking_region_counts` exact. No test's pinned counts move.

### 4. Gate before step 3's flip

Step 3's flip also needs `r13w5-ffm3-...`'s cost gate (2): the per-call cost of the region.
The region costs a TLAB retire, a full root deposit in, and a full wake out, per downcall. The
cheaper transition is `docs/known-issues/gc/gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`
section 3, the JNI bracket's quiet leave (`native_call_leave_native` with a `NativeSync` read
before the call, which skips the wake when no pause ran). It could be offered to FFM as a second
pair `begin_native_region_quiet() -> u64` / `end_native_region_quiet(u64)` over
`native_call_enter_native` / `native_call_leave_native` (both are private to `native/jni.rs` today:
make them and `NativeSync` `pub(crate)`). Measure with
`tools/bench/Gcd5FfmDowncallBlocksGcProbe.java`'s `[gcd5-ffm-cost]` stderr line, flag off vs on,
before shipping the flip.

## How to confirm

* A unit test in `vm` (or a probe): a thread sleeping in a downcall under the flag reports
  `Thread.State.RUNNABLE` from another thread.
* `C:\craton\jitr14-probes\src\R14FfmBlockedDowncallPause.java` prints `gc-progress ok` with the
  flag on (default after step 3), `gc-progress stalled rounds=0` with `=0`.

## Round 14 wave 1 (lane ffm): steps 1-3 applied, the flip held back

On the orchestrator's instruction, steps 1-3 are applied. The reader is NOT flipped:
`ffm_downcall_gc_safe` stays `runtime_flag_on` (opt-in), so both arms can be measured on one
build.

* `native-api/src/registry.rs` ~5219-5230: `NativeThreadAccess::begin_native_region` (default
  delegates to `begin_blocking_region`).
* `vm/src/vm/vm_exec.rs` ~20592-20597 (`impl NativeThreadAccess for NativeContextImpl`, after
  `begin_timed_blocking_region`): `begin_blocking_region_with_state(0)`.
* `native-builtins/src/panama.rs` ~7906-7911 (`pe_downcall_invoke`, `if gc_safe`) and
  `native-builtins/src/panama_libffi.rs` ~1720-1722 (`reenter_downcall_region`) now call
  `begin_native_region`.
* The flag row is unchanged (still opt-in).

Step 4 belongs to the orchestrator:
1. The cost A/B, `[gcd5-ffm-cost]` from `tools/bench/Gcd5FfmDowncallBlocksGcProbe.java`, flag
   off vs `=1`, 5 interleaved runs.
2. `C:\craton\jitr14-probes\src\R14FfmBlockedDowncallPause.java` in both arms (`gc-progress
   stalled rounds=0` vs `gc-progress ok`).
3. A thread in a downcall under `=1` now reports RUNNABLE.
4. If the cost passes, flip the reader to `runtime_flag_default_on` and set the row's
   `off_word: Some("0")`.

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
