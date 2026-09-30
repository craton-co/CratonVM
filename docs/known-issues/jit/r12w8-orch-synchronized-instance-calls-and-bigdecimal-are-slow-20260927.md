# A compiled caller's `synchronized` instance call costs ~400 ns, and `BigDecimal` runs ~1000x HotSpot

Status: OPEN
Area: `vm/src/jit/helpers.rs` (`try_jit_virtual_bytecode_callee`), `vm/src/runtime/interpreter.rs` (`install_and_run_cached_frame_monitored`), `vm/src/runtime/interpreter/jit_bridge.rs` (`door_locked_wrapped_body`, `execute_wrapped_body_door_locked`), the single-pass `sp-sync-direct-call` route; `java.math` on the JIT
Severity: HIGH (performance; no wrong answer)
Found by: round 12 wave 8 probes (`R12MonitorContended`, `R12Rt3FmaEdges`), analysed by the round 12 orchestrator close-out

## 1. `synchronized` instance methods called from compiled code

`R12MonitorContended` `sync-method-1t`: a four-field `synchronized int step()` (every 8th call through
`synchronized stepNested()` and `Thread.holdsLock`), plus a `static synchronized` twin, 1 M calls per
round. Standalone repro (`SyncM`, quiet host, ms per 1 M calls):

| binary | `step()` only | `step()` + nested + static |
|---|---|---|
| wave 8 (`w8b`) | 528-574 | 639-1374 |
| close-out (`w8e`) | 365-441 | 535-652 |
| HotSpot 25 | -- | 33-36 |

**What the close-out found.** The call runs `helpers.rs` `try_jit_virtual_bytecode_callee` (a
compiled caller's virtual call with no compiled entry in its inline cache: the only body the JIT
publishes for an `ACC_SYNCHRONIZED` method is a wrapped entry, which no raw door may call). That door
took the monitor and INTERPRETED the template on every call; `CRATONVM_DBG_JITC=1` printed a
`bc-callee-tiered-enqueue SyncM.step()I` line every 64 calls for the whole run (78 000 lines). Two
gates kept the compiled body out: `door_may_enter_wrapped_body` admitted static templates only, and
`optimizing_exits_route_exactly` refused an IR body with no recorded exit bcis even when the method
has no exception table. The close-out admits instance templates
(`CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY`, default on) and handler-free methods. That is the ~30%
above.

**What is left.** ~400 ns per call is now the round trip itself: compiled caller -> Rust helper ->
`VIRTUAL_BYTECODE_CALLEE_CACHE` probe -> `door_monitor_acquire` -> `execute_wrapped_body_door_locked`
(argument pins, a `JitSynchronizedMonitorGuard`, `run_jit_body`) -> release. A `static synchronized`
callee avoids all of it through the single-pass `sp-sync-direct-call` (a direct CALL with the caller
holding the class mirror's monitor; `jitc` shows `sp-sync-direct-call SyncM.staticStep()I`). No
instance twin exists: the inliner refuses `synchronized` callees (`inline-resolve REFUSED ...
synchronized`), and the direct-call route is `invokestatic` only. Options, cheapest first:

1. An instance `sp-sync-direct-call` for a monomorphic `invokevirtual` / `invokespecial` site whose
   receiver class is guarded (the inline cache already guards it): the caller takes the receiver's
   monitor with the inline inflated/thin fast path it already emits for `monitorenter`, CALLs the
   wrapped body, and releases. The static route's caller-held-monitor exit rules carry over.
2. Inline `synchronized` callees in the optimizing tier with an explicit `MonitorEnter`/`MonitorExit`
   pair around the spliced body (nested-lock elision already exists for this shape,
   `CRATONVM_JIT_IR_NESTED_LOCK_ELIM`).

The probe's later rounds getting slower (r4 about twice r0 on `w8b`) did not reproduce as cleanly on
`w8e`; recheck after option 1 lands.

## 2. `BigDecimal` / `BigInteger`

`R12Rt3FmaEdges` `fma-rows` checks 8192 `fma` results against a `BigDecimal` reference: 28.7 s on
CratonVM against 26 ms on HotSpot, identical checksums; the timed `fma` loop itself is 61 vs 16 ms. So
the `BigDecimal` arithmetic (`new BigDecimal(double)`, `multiply`, `add`, `round(MathContext)`,
`doubleValue`) runs about 1000x HotSpot. Not investigated. First step: a standalone loop of those
five calls under `CRATONVM_DBG_JITC=1` and `CRATONVM_DBG=dispatch-tally` to see what stays interpreted, what
goes through a native shim, and whether `BigInteger`'s `int[]` kernels (`multiplyToLen`, `mulAdd`,
`squareToLen`: HotSpot intrinsics) are compiled at all.

## How to confirm

`SyncM` / `R12MonitorContended` `t-sync-method-1t-r*` within ~3x of HotSpot; a `BigDecimal` loop
within ~5x.

## Round 13 wave 1 (lane callcost) -- section 2

**The ~1000x is not BigDecimal arithmetic; it is `Math.fma` running its JDK fallback.**
`R12Rt3FmaEdges` takes `t0` AFTER `build()`: the timed `fma-rows` loop is
`sum += row(i); sum += fixed(i);` and contains no BigDecimal at all -- only
`Math.fma` / `StrictMath.fma` (about 25 per iteration, 73 728 iterations: ~16 us per `fma`
on CratonVM). The JDK body of `Math.fma` is the `@IntrinsicCandidate` fallback:
`new BigDecimal(a).multiply(new BigDecimal(b)).add(new BigDecimal(c)).doubleValue()` (the
float overload ends in `floatValue()`), on `fixed`'s operands (`Double.MIN_VALUE`,
`0x1p-1000`, `1e308`) exact values of hundreds of digits. HotSpot never runs it (interpreter
math entry, C1/C2 `VFMADD`). CratonVM had NO native for `Math.fma` (only `StrictMath.fma`, a
`Bridge` in `phases_late`, which loses to bytecode under `--jdk-only`), so the fallback ran
wherever the JIT's call-site intrinsic was not asked first: in the interpreter, on a
dispatched call, from a call spliced into another body (lowered as an ordinary call and
direct-bound to the COMPILED fallback --
`r13w1-callcost-jit-bridge-splice-keeps-static-intrinsics-patch-FIXED-20260928.md`), and -- R12's
case, below -- at a single-pass-compiled caller's OWN site.
The orchestrator's small-value loop (15 ms / 2000 against 5) confirms plain BigDecimal is
not the 1000x.

**The route R12 took: the single-pass method compile direct-binds BEFORE it asks the
intrinsic ladder.** In `jit/src/lib.rs` `try_compile_inner`'s single-pass invoke planning, a
statically bound site first tries `plan_inline`, then the eager direct bind
(`callee_compiler`, `if direct_jit_callee_calls_enabled && matches!(invoke_kind, 1 | 3)`),
and only after both the call-site intrinsic ladder (`try_resolve_intrinsic`). `callee_compiler`
refuses a callee with a registered native (`direct_bind_name_refusal` -> `NativeShadow`), which
is why `Math.sqrt`/`min`/`abs` (registered `Intrinsic`) and `StrictMath.fma` (a `Bridge` row)
reach their intrinsics. `Math.fma` had no native: its JDK body (no `new` refusal applies to a
bind) was compiled and every compiled `row`/`fixed` CALLed the BigDecimal fallback. The OSR
door (`jit_bridge.rs` ~3241) asks the static ladder before its callee compiles, which is why
`chain` (`fma-throughput`) was only 4x. Established by reading, not yet measured:
`R13CallcostBigFmaRowsAnatomy` times R12's four shapes apart, and with both switches at 0
(`CRATONVM_JIT_STATIC_INTRINSIC_FIRST=0 CRATONVM_MATH_FMA_NATIVE=0`) `rows-math`/`fixed-math`
should be the slow ones and `*-strict` not; `CRATONVM_DBG_JITC=1` should show the direct bind
of `java/lang/Math.fma`.

**Landed (1): the ladder first.** `try_compile_inner` computes
`static_intrinsic_site` (an `invokestatic` `try_resolve_intrinsic` matches) and skips both
`plan_inline` and the eager direct bind for it, so the ladder emits the intrinsic whether or
not a native is registered. Switch `CRATONVM_JIT_STATIC_INTRINSIC_FIRST` (default on; `0`
restores the old order). Beyond `Math.fma` it changes the sites of intrinsic statics with no
registered native (e.g. `Integer`/`Long` `rotateLeft`/`rotateRight`/`highestOneBit`/
`lowestOneBit` where unregistered), which were spliced or direct-bound to their Java bodies.

**Landed (2): a native.** `native-builtins/src/lang_math.rs` `register_math_natives` registers
`java/lang/Math.fma(DDD)D` and `(FFF)F` as leaf `NativeKind::Intrinsic` natives (stated at the
site with `register_with_kind`) backed by `phases_late::java_fma_f64/f32` -- the one-rounding
bodies the JIT helpers and `StrictMath.fma` already use. `Math` only (`StrictMath.fma`'s body is
`return Math.fma(..)`). Kill switch `CRATONVM_MATH_FMA_NATIVE` (default on; `0` leaves the
JDK body). Effects: the interpreter and every dispatched call get a native instead of the
BigDecimal chain; `direct_bind_name_refusal` now refuses to bind the compiled fallback
(`NativeShadow`), so a single-pass splice of such a helper is refused whole and the helper
keeps its own intrinsic; IR-tier splices dispatch to the native; and even with (1) switched
off a compiled caller's own site reaches the intrinsic (the bind is refused). `invokestatic`
is not scanned by the native-shadow caller seal, so no caller is sealed out of the JIT. Unit tests `math_fma_is_an_intrinsic_on_math_only_and_rounds_once`,
`math_fma_native_has_a_kill_switch`. `stub_ratchet`'s `BASELINE_INTRINSICS` moves +2.

**BigDecimal / BigInteger on wide values, read while looking.** CratonVM serves
`BigDecimal` `<init>(D)`, `add`, `multiply`, `doubleValue`, `signum`, `compareTo`, `precision`,
the `MathContext` overloads and `BigInteger.pow`/`multiply` kernels from exact Rust limb
natives (`math_bignum.rs`, `bigint.rs`, `biginteger_intrinsics.rs`; Knuth D division, no
decimal round trips except below). Not 1000x anywhere by reading; the residual costs are
per-call native funnels, `bd_to_f64`/`bd_to_f32` rendering the whole unscaled value to a
decimal string and re-parsing it (O(digits^2) on a 750-digit value), `bigint_pow5` recomputed
per `new BigDecimal(double)`, and `BigDecimal.floatValue()` not registered in real-JDK mode
(runs the JDK body through `MutableBigInteger`). Filed as proposals in
`jit-r13-callcost-proposals-RETIRED-20260929.md`; `R13CallcostBigDecimalWide` and
`R13CallcostBigIntegerKernels` give a per-suspect ratio against HotSpot.

**Left.** The splice patch page above; the IR tier's own `fma` (`ScalarOp::FmaD/F`,
`jit-r12-rt-proposals.md` R12W8-1). Confirm: `R12Rt3FmaEdges` `fma-rows` within ~5x of
HotSpot's 26 ms with identical checksums, in the default arm and with EACH of the two switches
at 0 alone (either fix suffices for R12's shape); `R13CallcostBigFmaRowsAnatomy` and
`R13CallcostBigFmaSplice` in the same three arms plus both at 0 (the old behaviour).

## Round 13 wave 1 (lane sync) -- section 1

Status of section 1 unchanged: OPEN until the orchestrator measures `SyncM` /
`R12MonitorContended` `sync-method-1t` on the round-13 binary. Nothing here was
built or run by the lane.

**Landed: option 1, the single-pass INSTANCE caller-held synchronized direct CALL**
(switch `CRATONVM_JIT_SP_SYNC_DIRECT_INSTANCE`, default on; `CRATONVM_JIT_SYNC_DIRECT=0`
still turns off both twins). A compiled caller now CALLs a `synchronized` instance
callee's monitor-free wrapped body directly, holding the RECEIVER's monitor around
the CALL with the same inline thin/inflated sequence as a `monitorenter`, instead of
`jit_invoke_dispatch` / the MIC miss helper -> `try_jit_virtual_bytecode_callee` ->
`door_monitor_acquire` -> `execute_wrapped_body_door_locked`. The callee contract is
the static twin's, unchanged: `jit_bridge.rs` `sync_direct_target` answers only for
a published body that can neither trap, nor call, nor deopt, declared by the
Methodref's own class, with no exception table.

* Planning, `jit/src/lib.rs` `build_single_pass_tables` (after the static arm):
  an `invokevirtual`/`invokespecial` site whose target is such a body gets a
  `JitDirectCall` whose `guard_class_id` is `sync_direct_instance_guard(exact)`
  (tag bit `JIT_SYNC_DIRECT_INSTANCE_TAG` plus the exact receiver class, `0` = none)
  and its `JitInvokeInfo` (its own kind, 0 or 1). Statically bound (`invoke_kind == 1`,
  the VM's substituted owner IS the Methodref class; an `invokevirtual` also needs
  `unoverridable`): no class guard. Genuinely virtual (`invoke_kind == 0`, e.g. `SyncM`,
  a non-final class): CHA must find the Methodref class to be its own whole
  instantiable subtree (`compile_request::cha_sync_exact_class`, the IR tier's rule)
  and the site is guarded on exactly that class; no dependency is recorded (a
  receiver of that class selects that declaration whatever loads later).
* The single-pass OSR door (`jit_bridge.rs` `compile_osr_body`, after its static
  block) plans the same row for an `invokevirtual` it left on its inline cache:
  statically bound when `unoverridable` (a final class such as
  `R12MonitorContended.Counter`), else behind the CHA exact-class guard; the miss
  edge is the site's unchanged MIC/PIC dispatch.
* Lowering, `jit/src/x64/op_invoke.rs`: `walk_invoke_instance` null-checks the
  receiver at the invoke (statically bound; JVMS 6.5, before any monitor action) or
  emits `TEST; JZ miss; CMP DWORD [recv], exact; JNE miss` (guarded; the miss runs the
  ordinary dispatch and the hit joins it through `reconcile_guarded_inline_join`),
  then hands the site to `walk_invokestatic`'s caller-held arm, which enters a copy
  of the receiver pushed above the arguments, CALLs the entry, and on both edges
  (normal, and the merged-sentinel cold side before the exception route) releases
  the receiver read back from argument 0's word of the direct-call service range.
  That word is named in the CALL's oop map, so a moving collection inside the callee
  rewrites it; the lowering therefore requires `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS`,
  an oop-marked receiver, an unprotected pc and `jit_args + 1` spill words
  (`sync_direct_instance_site`), and otherwise leaves the row to dispatch.
* Review finding fixed in the new code before it shipped: the guarded hit edge
  arrives at the join from its release with RAX clobbered while the dispatch edge
  ends with `MOV [result], RAX`; the adjacent-reload elision (`slot_mirror`) would
  have read the result from RAX on the hit edge. The join clears it for this twin.

**What it does not cover (the next ~400 ns).** A `synchronized` callee whose body
CALLS anything (`stepNested` -> `step`, `Thread.holdsLock`, a `StringBuffer` method
calling `ensureCapacity`) or can trap is still refused by `sync_direct_target` on
both tiers and keeps the helper round trip. That is most real synchronized methods.
The fix is a self-locking compiled body (HotSpot's shape: the callee's prologue takes
the monitor, its every exit releases it), proposal S13-1 in
`jit-r13-sync-proposals-RETIRED-20260929.md`; option 2 (IR inlining with an explicit
`MonitorEnter`/`MonitorExit`) is S13-2. The single-pass OSR door's statically bound
`invokespecial`/private-pinned sites (kind 1) are not planned there yet (S13-9).

**How to confirm.**
* `rg -n 'sync_direct_instance_guard|JIT_SYNC_DIRECT_INSTANCE_TAG' jit/src vm/src`.
* Unit test `jit/src/lib.rs` `r13_sync_direct_instance_guard_tests`; the codegen
  tests on `r13w1-sync-x64-tests-instance-sync-direct-codegen-patch-FIXED-20260928.md`.
* `CRATONVM_DBG_JITC=1` on `SyncM s` / `R12MonitorContended`: a
  `sp-sync-direct-call ... receiver-monitor ... exact_class_id=` line for
  `step()I` (or `sync-direct REFUSED ... step()I` naming why the body is not closed),
  and no `bc-callee-tiered-enqueue SyncM.step()I` stream.
* Probes `C:\craton\jitr13-probes\src\R13SyncInstanceCalls.java` and
  `R13SyncInstanceThreads.java` (expected output in their headers), default and with
  `CRATONVM_JIT_SP_SYNC_DIRECT_INSTANCE=0`; timings of `SyncM s` / `sync-method-1t`
  interleaved against the round-12 final binary.

## Round 13 wave 4 (lane sync2) -- section 1

Status of section 1 unchanged: OPEN until the orchestrator measures. Nothing was
built or run by the lane.

The orchestrator's w2b data (the wave-1 instance twin buys nothing measurable:
once `step()` has an IR body with deopt points the closed-body contract refuses
it, and `stepNested` calls) confirms the diagnosis above: every caller-held
route needs a callee that cannot stop, and real synchronized methods can.

**Landed: self-locking compiled bodies, stage 1** (proposal S13-1; switch
`CRATONVM_JIT_SELF_LOCKING_SYNC`, default on). An admitted `synchronized`
INSTANCE method (no exception table, no `monitorenter`/`monitorexit`, no store
to local 0, no loop, no `invokedynamic`: `jit/src/lib.rs`
`self_lock_admits_method`) is compiled by the method-entry single-pass door as
a body that enters `this`'s monitor after its prologue and releases it on every
exit (`jit/src/x64/frames.rs` `emit_self_lock_enter`,
`emit_self_lock_release_at_return`, `emit_self_lock_release_at_exit` inside
`emit_epilogue`; `x64/op_control.rs` return arms). The finished body must have
no deopt stub but the direct `/ by zero` throw (`x64/driver.rs`
`self_lock_exits_are_closed`), else the door recompiles the wrapped entry. It
is published with `CompiledMethod::self_locks_monitor` and, in `JitCache::put`,
`requires_wrapped_entry == false`, so inline caches (single-pass AND
optimizing callers) and the dispatch cache CALL it: no helper door, no
`execute_wrapped_body_door_locked`. It publishes no OSR entry (the OSR door
compiles its own body inside the interpreter frame that holds the monitor), and
an admitted method stays off the optimizing tier, whose body would be a wrapped
entry again (the `step()` refusal in the data above). `stepNested` (calls,
`Thread.holdsLock`), `step()`, `StringBuffer.append`, `Vector.add` are in the
admitted shape; `Hashtable.get` (a loop) is not yet.

**Required companion patch** (not this lane's file):
`r13w4-sync2-jit-bridge-self-locking-body-mic-probe-patch-FIXED-20260928.md` -- the
inline-cache fill refuses every synchronized method by name before its cache
probe; without the patch the self-locking body is published but not reached,
and the call is interpreted (slower than wave 3). Without the patch, default
the switch off.

**Left:** `r13w4-sync2-self-locking-bodies-with-deopt-exits-20260928.md`
(deopt exits with the monitor held, loops, the IR twin, `static synchronized`,
handler tables, the `WRAPPED_STATIC_CALLEE` memo, direct binds, the
interpreter's double lock).

**How to confirm.** Unit tests `x64::frames::r13_sync2_self_lock_shape_tests`,
`x64::driver::r13_sync2_self_lock_exit_tests`,
`r13_sync2_self_lock_admission_tests`. Probes
`C:\craton\jitr13-probes\src\R13Sync2SelfLock.java` and `R13Sync2Deopt.java`
(expected output in their headers) plus `R13SyncInstanceThreads`,
`R12Lock2Invariants`, in the default arm and with
`CRATONVM_JIT_SELF_LOCKING_SYNC=0`. Timings: `SyncM ns` and
`R12MonitorContended` `t-sync-method-1t` against w2b (600-990 / ~565 ms per 1 M;
HotSpot 31-41 / 27-34). `CRATONVM_DBG_JITC=1` names every refused body
(`self-lock REFUSED ... open exits (deopt stub reasons [...])`).

## Round 13 wave 6 (lane sync3) -- section 1

Status of section 1 unchanged: OPEN until the orchestrator measures (nothing
built or run by the lane). Stage 1 of the self-locking bodies was reviewed for
soundness (no defect found; two hardenings) and extended, each behind a switch:
loops (`CRATONVM_JIT_SELF_LOCKING_SYNC_LOOPS`: `Hashtable.get`'s shape is now
admitted), direct binds of a published self-locking body
(`CRATONVM_JIT_SELF_LOCKING_DIRECT_BIND`: statically bound `private` / `final`
/ `super.` calls get a baked CALL instead of `jit_invoke_dispatch`), and the
`WRAPPED_STATIC_CALLEE` memo is now dropped on publication. Details, pages and
what is left: `r13w4-sync2-self-locking-bodies-with-deopt-exits-20260928.md`,
"Round 13 wave 6 (lane sync3)". New probes `R13Sync3Loops`,
`R13Sync3DirectBind` (`C:\craton\jitr13-probes\src`).

## Round 13 close-out measurement (orchestrator, build w6c, Windows, 3 interleaved runs)

| arm | `SyncM ns` rounds r0-r4 (ms) | `R12MonitorContended` t-sync-method-1t-r0 / 4t | fib | hashmap |
|---|---|---|---|---|
| w2b (round 13 waves 1-2) | 586-645 | 194-582 / 259-482 | 2935-2977 | 1856-1870 |
| **w6c (self-locking bodies)** | **112-139** | **84-109 / 133-139** | 2953-3606 | 1876-2479 |
| w6c, `CRATONVM_JIT_SELF_LOCKING_SYNC=0` | 586-692 | 545-551 / 479-496 | 2939-3002 | 1878-2009 |
| HotSpot 25.0.3 | 31-38 | 26 / 67-81 | 2098-2184 | 588-604 |

Section 1 is ~5.2x faster (590 -> 113 ms a round; HotSpot 31): the ~400 ns helper door is gone for
the admitted population; what is left (~3.6x HotSpot) is the inline thin-lock path itself and the
populations the self-lock still refuses (see `r13w4-sync2-self-locking-bodies-with-deopt-exits-20260928.md`).
Section 2 (BigDecimal) is unchanged by round 13. The r1 fib/hashmap outliers of the w6c arm are
first-run noise; r2/r3 match w2b.

## Round 13 wave 8 (lane bigdec) -- section 2

**The ~1000x of section 2 is gone; nothing else in `java.math` was found at that order on the
paths the section names.** The w7m default-arm run (`C:\craton\jitr13-probes\res-w7m-def.txt`)
has `R12Rt3FmaEdges` `fma-rows` at 117 ms against HotSpot's 26 (was 28.7 s): wave 1's `Math.fma`
native and ladder-first order (above) were the whole of it. `R13CallcostBigIntegerKernels` and
`R13CallcostBigDecimalWide` on the same run are 2-9x HotSpot per kernel (`tostring` faster), not
1000x. Under `--jdk-only` every HotSpot `BigInteger` intrinsic kernel is already an `Intrinsic`
native (`implMultiplyToLen`, `implSquareToLen`, `implMulAdd`/`mulAdd`, `implMontgomeryMultiply`/
`Square`, `shiftLeftImplWorker`/`shiftRightImplWorker`, `biginteger_intrinsics.rs`), and
`Math.multiplyHigh`/`unsignedMultiplyHigh`/`Long.numberOfLeadingZeros` are JIT call-site
intrinsics (`jit/src/lib.rs` `try_resolve_intrinsic`), so no kernel loop is left interpreted.

What reading the `Intrinsic` natives against JDK 25.0.3's `BigDecimal.java` did find, and wave 8
fixed (each behind its own default-on switch, all detailed in
`r13w8-bigdec-bignum-natives-quadratic-paths-and-wrong-mathcontext-divide-FIXED-20260928.md`):

* `divide(BigDecimal, MathContext)` gave a WRONG answer when the integer quotient already had more
  digits than the precision (`12345/7 @2` = `1.76E+3`, JDK `1.8E+3`) and cost one pass per digit;
  now one normalised division (`CRATONVM_BIGDECIMAL_DIVIDE_MC_ONESHOT`).
* `pow(int, MathContext)` built the exact power (O(n^2)) and rounded once -- not the JDK's X3.274
  answer, and `n < 0` threw; now the JDK's algorithm (`CRATONVM_BIGDECIMAL_POW_MC_JDK`).
* `toString()` re-rendered the whole value on every call (HotSpot caches: a repeated `toString()`
  of a 2000-digit value is the real ~1000x+ in this family) and interned it into a strong,
  never-pruned root; now `stringCache` + uninterned (`CRATONVM_BIGDECIMAL_TOSTRING_CACHE`,
  `CRATONVM_BIGNUM_STRINGS_UNINTERNED` for `BigInteger.toString()`/`toPlainString()`).
* Limb products were schoolbook only; now Karatsuba from 80 limbs, and `BigInteger.pow` factors
  the base's trailing zero bits like the JDK (`CRATONVM_BIGINT_FAST_MUL`).

Section 2 can be retired once the orchestrator confirms `R12Rt3FmaEdges` (above) and the new
probes `R13BigdecMathContext` / `R13BigdecHuge`; section 1 keeps the page OPEN. Left:
`r13w8-bigdec-bignum-residuals-FIXED-20260929.md`, `r13w8-bigdec-dynamic-strings-interned-into-a-never-pruned-root-FIXED-20260929.md`,
`jit-r13-bigdec-proposals-RETIRED-20260929.md`.

## Round 13 wave 10 (lane monitor2) -- section 1, the monitor part

Status of section 1 unchanged: OPEN (nothing built or run by the lane; the
close-out measured `SyncM ns` at 112-139 ms a round against HotSpot's 31-38).
Read against the w9 tree (`1e4c0055c`), the monitor sequences are not where the
remaining ~3.6x is:

* The uncontended self-locking body's enter and exit are the inline thin path
  (`jit/src/runtime_lowering.rs` `emit_inline_thin_lock`): one `LOCK CMPXCHG`
  of the mark word each way, the lease count as a plain `ADD` / `SUB`, the
  four-store seqlock push / pop of the JMX lock stack, the thread read twice
  from TLS -- about 30 instructions and one locked op each way, with the
  re-entry of `stepNested -> step` inline since wave 8 (the recursion arm, one
  `LOCK CMPXCHG` of the mark's count, no lock-stack write). HotSpot's
  lightweight locking is one CAS and a two-store lock-stack push each way;
  the difference is the seqlock (a `JmxLockStack` protocol with the VM's JMX
  readers, `thread_registry.rs`, not this lane's) and a handful of loads --
  nanoseconds, not the ~80 ns per iteration the gap is.
* `Thread.holdsLock` (every 8th `stepNested`) on a THIN receiver never touches
  the index; on an inflated one it now reads the thread's monitor cache
  instead of taking a shard lock (`CRATONVM_MONITOR_CACHED_NOTIFY`, this wave).
* The rest is call cost and the populations the self-lock refuses, both owned
  elsewhere this wave (`frames.rs` / `driver.rs` / `lib.rs`: lanes
  callcost4 / sync6); `static synchronized` is the static self-locking design
  (`r13w8-sync5-static-synchronized-self-locking-design-FIXED-20260929.md`).

What this wave added that a synchronized-method workload can reach: a
receiver alternating with ANOTHER inflated monitor (a synchronized method on a
hashed object calling a `static synchronized` method of a class whose mirror
is inflated, or nested synchronized calls on two once-contended receivers)
stays on the inline arm for both
(`CRATONVM_JIT_INLINE_INFLATED_TWO_WAY`; see the where-a-contended-enter page,
"Round 13 wave 10"). Probe `R13Monitor2Contended` `method-<T>t` gives the
1/2/4/8-thread synchronized-method timings next to the block timings.

## Round 13 wave 11 (lane sync7) -- section 1

Status of section 1 unchanged: OPEN until the orchestrator measures `SyncM` (nothing built or
run by the lane).

* **`SyncM`'s `staticStep` half, default arm:** the caller-held route of a `static synchronized`
  callee (`sync_direct_target`) baked `mirror_slot = 0` whenever the caller's own `ldc` site for
  the Methodref class had never been executed, which is always true of `SyncM.main`'s OSR body
  (nothing `ldc`s `SyncM.class`): every `staticStep()` call paid a `jit_ldc_class_cp` helper
  CALL (spill, oop map, `contain`, memo probe) to fetch the monitor. Wave 10's per-class mirror
  slot fallback (proposal S5-1) existed but only under default-OFF
  `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`; it is now on by default
  (`CRATONVM_JIT_SYNC_DIRECT_CLASS_MIRROR_SLOT`), and the single-pass OSR door compiles on the
  mutator, which mints the slot from the existing mirror. Expect the `s` / `ns` modes to lose
  one helper call per iteration; `CRATONVM_DBG_JITC=1` shows `sp-sync-direct-call
  SyncM.staticStep()I ... mirror_slot=0x<non-zero>`. A/B: `CRATONVM_JIT_SYNC_DIRECT_CLASS_MIRROR_SLOT=0`.
* **Static self-locking bodies** (the route for static synchronized methods that call or
  allocate) are complete behind their opt-in switch, deopt hand-over included; see
  `r13w8-sync5-static-synchronized-self-locking-design-FIXED-20260929.md` ("Round 13 wave 11").
* What is left of the ~3.6x on the instance half is call cost and the inline monitor sequence
  (wave 10 lane monitor2's reading above); proposals S7-4 and S7-5 in
  `jit-r13-sync7-proposals-RETIRED-20260929.md` name two monitor-sequence cuts worth measuring.

## Round 13 wave 12 (lane bigdec3) -- section 2

Nothing built or run by the lane. **Section 2's own claim is met by the numbers already on
disk:** `R12Rt3FmaEdges` on w11a (`C:\craton\jitr13-probes\res-w11a-def.txt`) is 140 ms against
HotSpot's 24 (was 28.7 s; the ~1000x was `Math.fma`'s fallback, wave 1). What remains is the
per-shape gap of `R13CallcostBigDecimalWide` (w11a vs `ref13`, ms): `fma-small` 312 / 92 (3.4x),
`fma-wide` 399 / 109 (3.7x), `fma-tiny` 1757 / 262 (6.7x), `roundtrip-wide` 86 / 16 (5.4x),
`roundtrip-tiny` 572 / 56 (10x), `floatvalue-small` 478 / 20 (24x), `compare-wide` 406 / 42
(9.7x). Section 2's confirm line ("a `BigDecimal` loop within ~5x") holds for the small and wide
arithmetic shapes, not yet for the tiny-value, `floatValue` and `compareTo` shapes.

Landed this wave against those shapes (details on
`r13w8-bigdec-bignum-residuals-FIXED-20260929.md`, "Round 13 wave 12"):
* `doubleValue()` without the decimal rendering (`CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE`):
  `fma-tiny` and `roundtrip-tiny` rendered a 700-digit value to a `String` and parsed it per call.
* `compareTo` decided from bit lengths when the magnitudes cannot overlap
  (`CRATONVM_BIGDECIMAL_COMPARE_BY_BITS`), and digit counts without a power of ten for compact
  values and for ~70% of wide ones (exact, no switch): `compare-wide` built two powers of ten per
  call, and `precision()` one.
* `add`/`subtract`/`multiply`/`negate`/`setScale` return the JDK's cached constants where it does
  (`CRATONVM_BIGDECIMAL_RESULT_CONSTANTS`; identity, and one allocation less).

Left, as proposals in `jit-r13-bigdec3-proposals-RETIRED-20260929.md`: `floatvalue-small` (24x) is the JDK's
`floatValue` bytecode because real-JDK mode registers no `floatValue` native (BD3-1, which moves
`stub_ratchet`'s intrinsic ceiling); `roundtrip-tiny` / `fma-tiny` rebuild `5^n` in the
constructor and again in `doubleValue` (BD3-2, a per-VM power cache like the JDK's
`BIG_TEN_POWERS_TABLE`). New probe `C:\craton\jitr13-probes\src\R13Bigdec3FiveCalls.java` times
section 2's five-call shape directly (`fma-shape`, `ffma-shape`, `round-mc`). Section 2 can be
retired when the orchestrator re-runs `R13CallcostBigDecimalWide` on the wave-12 build; section 1
keeps the page OPEN.

## Round 14 wave 1 (lane sync) -- section 1

Status of section 1 unchanged: OPEN (nothing built or run by the lane; the last measurement is
`SyncM` ~110 ms a round against HotSpot's ~32, w11a). Re-read against `adb9178bc`: what is left
of `SyncM` is the CALL (both callees are leaf bodies HotSpot inlines), which is proposal SR-1 of
`jit-r13-syncres-proposals-RETIRED-20260929.md` (splice a trap-free synchronized callee between `MonitorEnter` and
`MonitorExit`); its touch points are `jit_bridge.rs` `resolve_inline_site_from` (interpreter
round's file this round) and the IR builder (`ir.rs`, lane chain), so it was not attempted here.
The two queued self-lock flips (`CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER`, then
`CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`) are the next measurable step; nothing found this wave
argues against them (see the wave-14 section of
`r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md`).

Landed this wave for the population the self-lock never serves (synchronized methods WITH an
exception table whose handler reads a non-parameter local -- every synchronized method that
contains a `synchronized` block, and every `catch` that reads a local set in its `try`):

* **Backlog #9, second half (irexc proposal W11-2)**, behind default-OFF
  `CRATONVM_JIT_IR_PRECISE_FRAMES_SYNCHRONIZED` (`jit/src/lib.rs`
  `ir_precise_frames_synchronized_enabled`): such a method may take the optimizing tier's
  precise-frame route instead of staying single-pass. By reading it is sound (the argument is on
  the switch's reader: a wrapped body names no method monitor, the pads refuse virtual locals and
  relock monitors, and both handler sinks repair a dead slot 0 before the monitor re-acquire).
  Two guards make it no worse than today by construction: the post-lowering trap check now knows
  a synchronized method's guard exits resume only through the vouched arm
  (`ir_precise_frames_trap_post_check(.., is_synchronized)`, the sink's own
  `sink_precise_resume_allowed` rule), and a body the compiled caller's synchronized door could
  not enter (`jit_bridge.rs` `optimizing_exits_route_exactly`: an exit that publishes no frame
  inside a protected range) is discarded, so the door never falls back to INTERPRETING a template
  whose single-pass body it used to run. Test
  `r11w10_irexc_trap_post_check_tests::a_synchronized_body_leans_only_on_the_vouched_resume`.
* The first half of backlog #9 (r11-tier 46, the caller-held route for a callee with an
  exception table) was not attempted: its cold side is `op_invoke.rs` (lane calls) and
  `sync_direct_target` (`jit_bridge.rs`).

Found while reading the handler sinks (filed, LOW):
`r14w1-sync-handler-sinks-conflate-the-method-monitor-with-local-0-FIXED-20260929.md`.

How to confirm: `C:\craton\jitr14-probes\src\R14SyncIrPreciseFrames.java` identical to HotSpot in
the default arm and with `CRATONVM_JIT_IR_PRECISE_FRAMES_SYNCHRONIZED=1` (also with
`CRATONVM_C2_ACCEPT=always` and `CRATONVM_JIT_THRESHOLD=1`); `CRATONVM_DBG_JITC=1` with the switch
shows the optimizing tier taking `R14SyncIrPreciseFrames$Box.step` / `sstep` or, when it does not,
an `[ir] ...` discard line naming why; timings of its `sync-irprecise-*` lines in both arms.

## Round 14 wave 2 (lane bigdec) -- section 2

Nothing built or run by the lane. Section 2's own claim stays met (the ~1000x was `Math.fma`'s
fallback, round 13 wave 1). Landed this wave in the `java.math` natives, each exact and behind a
default-on switch:

* BD4-2: `lib.rs` `bigint_mul_pow10` (every `add`/`subtract` rescale, `setScale` raise, the
  `divide` roads) takes `5^n` from the per-thread memo (`math_bignum::bigint_mul_pow10_memo`;
  existing switch `CRATONVM_BIGNUM_POW5_CACHE`).
* BD4-1: `BigInt::from_decimal` nine digits per step and divide and conquer from 1152 digits
  (`CRATONVM_BIGINT_FROM_DECIMAL_FAST`), also behind `bi_alloc`'s `decimal_to_mag_words`.
* `bi_read`'s `mag_words_to_decimal` (one long division per DIGIT) is the limb conversion
  (`CRATONVM_BIGINT_MAG_TO_DECIMAL_CHUNKED`); `--compatible`'s `gcd` (decimal Euclid, O(digits^3))
  and `toByteArray` helpers take limb roads past 18 digits (`CRATONVM_BIGINT_STR_HELPERS_LIMB`).

Found by reading: in the default real-JDK mode `new BigInteger(String)` / `new BigDecimal(String)`
never reach a native (only the synthetic-jdk registrars register `<init>(String)`), so Java-level
parsing is JDK bytecode; proposals BD5-1/BD5-2 in `jit-r14-bigdec-proposals.md`. New pages:
`r14w2-bigdec-phases-late-biginteger-decimal-roundtrips-patch-FIXED-20260929.md`,
`r14w2-bigdec-synthetic-biginteger-ctor-rejects-unicode-digits-FIXED-20260929.md`. Probe
`C:\craton\jitr14-probes\src\R14BigdecParse.java` (parse / format / radix / bytes / gcd / rescale /
NumberFormatException rows, 1..2000 digits) sizes the Java-level parse gap for BD5-1.

## Round 14 wave 3 (lane monitor) -- section 1, re-measured by reading

Status of section 1: OPEN, narrowed to a measurement (nothing built or run by the lane; the last
number is `SyncM` ~110 ms a round against HotSpot's ~32, w11a, before wave 2). By reading
`20a1dbb4f`, `SyncM`'s three callees now split three ways:

* **`step()`** (the `step`-only and `ns` modes): exactly the shape wave 2's synchronized splice
  (SR-1, `CRATONVM_JIT_IR_SYNC_SPLICE`) admits -- `jit/src/lib.rs` `ir_sync_splice_body_scan`'s own
  unit test `r14_syncsplice_scan_tests::STEP` is this method's bytecode (getfield / putfield through
  the receiver, one forward branch, one trailing `ireturn`). Where the caller is compiled by the
  optimizing tier, the CALL is gone and what is left is one inline monitor enter/exit per call,
  which is also what HotSpot pays when it inlines a synchronized leaf (lock coarsening aside).
  Whether `SyncM.main`'s loop reaches that tier (it is an OSR body) decides whether `SyncM`
  measures it: `CRATONVM_DBG_JITC=1` shows either the splice or an `ir-sync-splice-*` refusal.
* **`staticStep()`** (the `s` mode): admitted by the same scan (own-class statics, non-volatile
  writes), but wave 2 flipped `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC` on, and a static splice then
  needs the class mirror without the caller-held row -- proposal SS-2, lane sync's this wave. Until
  it lands, the static half runs the self-locking body through the caller-held route (a CALL).
* **`stepNested()`** (every 8th call in the `n` mode): calls `Thread.holdsLock`, so the scan refuses
  it (`ir-sync-splice-body-calls`); it stays a self-locking body CALL, whose own `holdsLock` is a
  native call inside the critical section (M3-1 / S7-1, the `holdsLock` fold, not landed).

What would close section 1: `SyncM` `ns` / `s` / `n` (and `R12MonitorContended`
`t-sync-method-1t`, `R13Monitor3Uncontended`) interleaved, default against
`CRATONVM_JIT_IR_SYNC_SPLICE=0`, on a build with SS-2. Expect `ns` near the uncontended inline
monitor cost (a few ns a call) and within ~2x HotSpot; if it is, retire section 1 with the
`stepNested` residual moved to a `holdsLock`-fold page. Section 2 was already met (see the round
13 wave 12 section), so the page could then be retired whole.

## Round 14 wave 4 (lane sync4) -- section 1

Status of section 1: OPEN, still a measurement (nothing built or run by the lane). Re-read against
`20a668e12` (waves 1-3 landed). What keeps `SyncM` (`scratch-if/SyncM.java`) off HotSpot, per mode:

* **`s` / `ns` (`step()`, `staticStep()`):** both bodies are in the synchronized splice's admitted
  shape (`step` is `r14_syncsplice_scan_tests::STEP`; `staticStep` takes the class mirror from the
  lookup's monitor half since SS-2). Whether they ARE spliced depends only on `SyncM.main`'s loop
  reaching the optimizing tier (it is an OSR body; the single-pass OSR door keeps the self-locking
  body CALL). This wave adds the census that says so without parsing `jitc` output: every built
  splice prints `[cratonvm-jitc] ir-sync-splice built instance|static-row|static-mirror at pc=<n>
  [in-region]`, and `r14w4-sync4-interp-census-sync-splice-line-patch-FIXED-20260929.md` (exact patch,
  `interp_census.rs`) prints the per-kind totals at exit (SY3-5).
* **`n` (`stepNested()` every 8th call):** still a self-locking body CALL, and SS-7 (landed this
  wave, below) does not change that by itself. `stepNested` is
  `if (!Thread.holdsLock(this)) return 1; return step();`, refused three ways: two `ireturn`s (the
  resolver's and the scan's single-trailing-return rule), a nested synchronized call (the resolver
  answers no synchronized body at depth > 0, and the scan admits no call but a folded `holdsLock`),
  and -- until this wave -- the `holdsLock` itself. The remaining two are proposal SS8-1 in
  `jit-r14-sync4-proposals.md` (a same-receiver nested synchronized splice needs NO pair: the
  outer window holds the receiver, the SY3-2 argument; plus a multi-return synchronized splice
  whose exit sits after the join). Its own CALL costs one inline recursive thin-lock CAS pair and
  the call; at one call in eight it is not the bulk of the gap.

Landed this wave (each behind a default-ON kill switch, pending build):

* **SS-7, `Thread.holdsLock(o)` folded to `1`** where the IR builder proves the hold
  (`ir.rs` `IrBuilder::try_fold_holds_lock`, `CRATONVM_JIT_IR_HOLDSLOCK_FOLD`): `o` is the receiver
  of an open instance synchronized splice, or an entry of the compiling method's own monitor
  stack. The planner (`lib.rs` `ir_sync_splice_body_scan`) now admits a synchronized body whose one
  call is `holdsLock(this)`, and skips the replay fence and the unbindable-call refusal for such a
  body (`ir_sync_splice_folds_every_call`). A body the planner admitted and the builder did not
  fold builds a CALL inside the window, which `end_splice` refuses as a site (never a call under
  the splice's monitor).
* **SY3-2, a splice's pair inside a region on the same object deleted whole**
  (`ir_optimize.rs` `elide_region_nested_sync_windows`, `CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_ELIM`,
  also under `CRATONVM_JIT_IR_NESTED_LOCK_ELIM`): `synchronized (o) { o.leaf(); }` now costs the
  region's pair only (the `Hashtable`/`Vector` "lock around own calls" shape).
* **SY3-5, the census** (above).

How to confirm: unit tests `ir::r14w4_sync4_builder_tests`,
`ir_optimize::r14w4_sync4_window_elision_tests`, `r14w4_sync4_holds_lock_scan_tests` (`lib.rs`);
probes `C:\craton\jitr14-probes\src\R14Sync4HoldsLock.java` and `R14Sync4RegionDeopt.java`
(expected output and arms in their headers). Then the measurement the wave-3 section names:
`SyncM` `s` / `ns` / `n` with `CRATONVM_DBG_JITC=1` (look for `ir-sync-splice built` lines for
`step`/`staticStep` in `SyncM.main`), default against `CRATONVM_JIT_IR_SYNC_SPLICE=0`.

## Round 14 wave 5 (lane sync5) -- section 1

Status of section 1: OPEN, still a measurement (nothing built or run by the lane). Landed this
wave, each behind a default-ON kill switch, pending build:

* **SS8-3, the compiling synchronized method's own monitor.** An `ACC_SYNCHRONIZED` instance
  method's optimizing compile records `Graph::method_monitor_param = Some(0)` (`lib.rs`, next to
  `receiver_param`; switch `CRATONVM_JIT_IR_SYNC_METHOD_MONITOR_FACTS`): every entry into such a
  body holds the receiver's monitor (the door's wrapped entry, stamped `requires_wrapped_entry`
  at every publication, or an OSR transfer from the interpreter frame that holds it), and the
  graph cannot release it (`monitorexit` must match the innermost bytecode monitor or the build
  is refused). So `Thread.holdsLock(this)` folds to `1` anywhere in the body
  (`IrBuilder::try_fold_holds_lock`), and a synchronized splice locking `this` there --
  `synchronized putAll -> put`, `Vector.addAll`-style chains -- loses its recursive pair
  (`ir_optimize::elide_region_nested_sync_windows`, no snapshot needed). Census rows
  `holdslock-folded-in-method`, `window-elided-in-method`.
* **SS8-1 (b), a multi-return synchronized splice.** `ir_sync_splice_body_scan` admits several
  returns, `append_ir_inline_site` no longer refuses such a body, and the builder emits the
  window's exit after `finish_multi_return_splice`'s join (switch
  `CRATONVM_JIT_IR_SYNC_SPLICE_MULTI_RETURN`). Inert in the default arm until the resolver
  answers such bodies: exact patch
  `r14w5-sync5-resolver-sync-splice-multi-return-patch-FIXED-20260929.md` (or run with
  `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1`). Census row `multi-return`.
* **A wrong answer in wave 4's `holdsLock` fold, fixed**: an unpatched loop-header φ read as the
  held object (`r14w5-sync5-holdslock-fold-trusts-an-open-loop-phi-FIXED-20260929.md`).

Not landed: SS8-1 (a), the nested same-receiver synchronized call (`stepNested -> step`), and
SS8-5, `holdsLock(C.class)` in static splices -- both need resolver rows; plans in
`r14w5-sync5-sync-splice-resolver-residuals-FIXED-20260929.md`. So `SyncM`'s `n` mode still pays one
self-locking CALL per eighth call; `s`/`ns` are unchanged from wave 4 (their bodies were already
in the admitted shape; whether `SyncM.main`'s OSR loop reaches the optimizing tier is still the
open measurement).

How to confirm: unit tests `ir::r14w5_sync5_builder_tests`,
`ir_optimize::r14w4_sync4_window_elision_tests::a_window_on_the_synchronized_methods_receiver_is_deleted`,
`r14_syncsplice_scan_tests::traps_calls_loops_and_second_returns_are_refused` (`lib.rs`); probe
`C:\craton\jitr14-probes\src\R14Sync5MethodMonitor.java` (expected output and arms in its
header); then the wave-3 section's `SyncM` measurement.

## Round 14 wave 6 (lane monitor3) -- the monitor side

Status of section 1 unchanged (a measurement; nothing built or run by the lane). Nothing on the
UNCONTENDED path moved this wave: `SyncM`'s monitor cost is the inline thin sequence
(`runtime_lowering.rs`, both tiers) or, where the splice lands, nothing but one pair per call, and
neither passes through `vm/src/threading/monitor.rs` on the hot path. What landed in `monitor.rs`
is contended-only (census hooks for the crowd and the park length, and the opt-in
`CRATONVM_MONITOR_SPIN_HANDOVER_ABORT`, inert without `CRATONVM_MONITOR_LAZY_PARK_US`); details on
`r12w8-monitor-where-a-contended-enter-spends-its-time-20260927.md`, "Round 14 wave 6". For
`R12MonitorContended` `t-sync-method-4t` (the contended synchronized method), the decisive run is
that page's census set; the monitor-side cost of section 1 is otherwise closed by reading, as
round 13 wave 10 concluded, pending `R13Monitor3Uncontended` (M3-2).

## Round 14 wave 6 (lane sync6) -- section 1

Status of section 1: OPEN, still a measurement (nothing built or run by the lane). Landed, each
behind a default-ON kill switch, pending build:

* **SS8-1 (a), `SyncM`'s `n` mode.** `stepNested -> step` on the same receiver: the resolver
  answers `step` nested under `stepNested`, the planner admits it, and the builder splices it
  inside `stepNested`'s window with NO pair of its own, only where the receiver node is proven
  the window's monitor (`CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_SAME_RECEIVER`). With SS8-1 b (wave
  5, the two returns) and SS-7 (the `holdsLock(this)` fold) this removes the last refusal named
  in the wave-4 section for `stepNested`: the `n` mode should now build one window per call and
  no CALL. Details: `r14w5-sync5-sync-splice-resolver-residuals-FIXED-20260929.md`, round 14 wave 6.
* **S5-2**, `holdsLock(this)` folded on the finished graph of a synchronized method
  (`CRATONVM_JIT_IR_HOLDSLOCK_METHOD_FOLD`), for the loop shapes the builder's fold gives up.

How to confirm: `SyncM` `n` with `CRATONVM_DBG_JITC=1` prints `ir-sync-splice nested-same-receiver
at pc=` and `ir-sync-splice built instance` for `stepNested`, and no
`ir-splice-refused ir-sync-splice-body-calls` for it; then `SyncM` `n` / `ns` interleaved against
`CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_SAME_RECEIVER=0`. Probe
`C:\craton\jitr14-probes\src\R14Sync6NestedHeld.java`.
