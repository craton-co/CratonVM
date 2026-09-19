# Implicit exceptions thrown from compiled code cost 3-8 us each (HotSpot: ~0)

**Status:** PARTIALLY FIXED (round 9 wave 11, osr11): the OSR re-entry after a caught exception no longer re-runs the OSR door's gate chain (a validated per-thread memo). Still open: the handler itself runs in the interpreter (one exception-exit + one OSR entry per exception), and `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` stays default OFF. See "Status after wave 11".
**Owner-area:** the VM exception runtime (stack-trace construction for implicit
AIOOBE/AE/NPE raised by compiled code; `../../../vm/src/runtime/exceptions.rs`,
`../../../vm/src/jit/helpers.rs` throw helpers), plus the deopt -> precise-resume -> OSR
exception-exit round trip for a callee throw.
**Found by:** JIT review round 9, wave 8, lane `review8c`, 2026-09-18
(`C:\craton\jitr9-probes\review8c`, `ExcLoop`, `InlineDeoptProbe`). Filed by lane `wire8b`.
**Related:** `perf-a-classcastexception-costs-5ms-to-construct-20260918.md` (a different,
now-fixed cause: the reclaimed-memory forensics).

## Evidence (w7b, per exception, after warm-up)

| exception | HotSpot | CratonVM |
|---|---:|---:|
| local AIOOBE | 0 ns | 3 250 ns |
| callee AE | 1 ns | 5 163 ns |
| callee AIOOBE | 0 ns | 8 103 ns |
| preallocated `athrow` | 3 ns | 185 ns |

The preallocated row shows the catch/exit machinery costs about 0.1-0.2 us; the rest is
building the exception, mostly its stack trace, which HotSpot omits for hot implicit
exceptions (`-XX:+OmitStackTraceInFastThrow`).

`InlineDeoptProbe`: 55.6 s vs HotSpot 0.69 s. Dominated by this, plus a callee `div`
throw going `TransferToInterpreter` deopt -> helper precise-resume -> OSR exception-exit
-> interpreter -> OSR re-entry for every exception.

## Proposal

* An `OmitStackTraceInFastThrow` equivalent: after N implicit exceptions of one kind at
  one compiled site, throw a preallocated instance with an empty stack trace (behind a
  flag, since it is observable).
* For the callee-throw path, dispatch the exception to the compiled caller's handler
  without the deopt/resume/OSR round trip where the handler is in compiled code.

## Also measured (detector coverage, not a defect)

`TryLoop plain` (`s += a[i] ^ i` with a `long` accumulator) runs 203 ms vs HotSpot's
29 ms (1.5 vs 0.22 ns per iteration): no SIMD shape matches `a[i] ^ i`.

## Status after wave 9 (lane `exc9`)

### Landed: an `OmitStackTraceInFastThrow` equivalent (opt-in)

* **Flag.** `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1` is the equivalent of
  `-XX:+OmitStackTraceInFastThrow`. **It is default OFF**, and off is
  `-XX:-OmitStackTraceInFastThrow`: every throwable is built in full, exactly as
  before. It stays off because the effect is observable (null message, empty
  trace), and this VM's stated rule is that a hot compiled throw and a cold one
  read the same (`../../../vm/tests/jit_npe_message_hot_equals_cold.rs`). The
  integrator should map `-XX:±OmitStackTraceInFastThrow` onto the flag in
  `vm-cli`; the request is in `NOTES-w9-exc9.md`.
* **Mechanism.** All of it is in `../../../vm/src/runtime/exceptions.rs`, section
  "OmitStackTraceInFastThrow":
  * `FastThrowKind`, `try_fast_throw`, `throw_implicit_runtime_error`,
    `create_implicit_exception_object`, `create_stackless_exception_object`
    and `ResumedCompiledFrameScope`.
  * A site is counted per thread. The key is `(vm, interpreter-frame code
    address, pc, kind)`, and the table is capped at 4096 entries.
  * After `FAST_THROW_HOT_THRESHOLD` (16) full throws at a site, each later
    throw gets a **fresh** instance, not one shared preallocated object. That
    instance has a null message, no `backtrace` and no entry in the VM trace
    store, so `getStackTrace()` returns `[]`. `cause` and
    `suppressedExceptions` are mirrored as a constructor would set them.
  * The fast path runs no constructor, walks no stack, formats no message and
    does no JEP 358 analysis.
* **Doors that use it.** Only drains that build an implicit exception for
  compiled code:
  * NPE, AIOOBE and ArithmeticException in each of the four `jit_bridge.rs`
    drains: the OSR bail, `execute_jit_call`, `execute_jit_call_decoded` and
    `execute_jit_call_oneshot`;
  * the same three in the `interpreter.rs` JIT early-throw drain;
  * `helpers.rs` `materialize_implicit_signal`, the callee-handler door;
  * the compiled `checkcast` CCE (`jit_checkcast`) and the compiled `aastore`
    ArrayStoreException (`aastore_store_is_refused`);
  * the helper precise-resume of a trapped callee, through
    `ResumedCompiledFrameScope` and a check at the top of
    `throw_runtime_error`. This is the `callee` row: the interpreter
    re-executes the trapping `idiv` of the deopted callee.

  Interpreter-raised exceptions for the interpreter's own bytecode are never
  made stackless, as in HotSpot.
* **Tests.** Unit tests in `exceptions.rs` `mod tests`:
  * `fast_throw_site_turns_hot_after_the_threshold_and_the_table_is_bounded`
  * `fast_throw_kind_covers_exactly_the_five_implicit_exceptions`
  * `resumed_compiled_frame_scope_nests_and_restores`
  * `stackless_exception_has_the_class_and_no_stored_trace`
  * `fast_throw_door_is_closed_when_the_flag_is_off`

### Measured baseline (not the fix)

The only binaries are the w8b and w7b builds, and both predate this change, so the
gain is **not measured**. Baseline, `ExcLoop 200000`, measured 2026-09-19, one
interleaved run each, third warm round, ns per exception:

| binary | local AIOOBE | callee AE | callee AIOOBE | preallocated |
|---|---:|---:|---:|---:|
| w8b | 4307 | 10746 | 9144 | 208 |
| w7b | 7078 | 9564 | 10046 | 195 |

To measure after the build, run `ExcLoop` (`C:cratonjitr9-probes
eview8ccls`)
twice, interleaved, with `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` set to `1`
and unset.

Expected result:

* `local` and `calleeIdx` should fall toward the `pre` row plus the drain and
  routing cost.
* `callee` keeps its deopt round trip (below), so it can only lose the
  construction share.

### Still open

* **The callee-throw round trip.** `CRATONVM_DBG_DEOPT=1` on `ExcLoop 20000` shows
  that every callee `div` throw is:
  1. `ExcLoop.div reason=TransferToInterpreter bci=2 action=Reinterpret`;
  2. `helper precise-resume of trapped callee`;
  3. `OSR exception-exit TRANSFER`.

  That is about 29 600 deopts for 30 000 throws, so the callee is re-entered
  compiled and deopted again every time. The fix is to dispatch the exception to
  the compiled caller's handler without the deopt, or to stop re-trapping. It
  lives in the JIT's zero-divisor lowering and the deopt policy, not in this
  lane's files.
* **The compiled-frame snapshot.** `snapshot_trap_frames` still runs at the trap
  on the fast path. It is taken before the drain knows the site is hot, and is
  then dropped.

## Integrator measurement after the wave-9 build (2026-09-19, `cratonvm-jitr9-w9.exe`)

`ExcLoop 200000`, 2 interleaved rounds, `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=0` vs `=1`:
local 2970-3020 vs 2978-3046 ns, callee 3918-4058 vs 3822-4145 ns, calleeIdx 3159-3358 vs
3237-3298 ns, preallocated 91-102 vs 91-109 ns -- **no difference**; HotSpot 396-434 /
690-728 / 396-400 / 27-59 ns. So either the fast-throw path does not engage on these sites
(check which builder the compiled `idiv`/`aaload` trap reaches with the flag on) or the
remaining ~3 us is not exception construction (the deopt / resume round trip the wave-9 notes
name for `callee`). The flag stays default OFF; next step is to find where the 3 us go.

## Status after wave 10 (lane `exc10`)

### Where the 3 us went (w9b, sampled + traced)

Measured with a native sampler on the main thread (`diag/sampler.py`, ntdll kept)
plus `CRATONVM_DBG_DEOPT` / `CRATONVM_DBG_OSR` / `CRATONVM_DBG_RTERR` traces:

* **No SEH / hardware trap** on any row: ntdll is ~1 % of the main thread.
* **The fast-throw door never opened.** Flag ON, a probe that counts
  `getMessage() == null` in the handler: 0 of 150 000 caught AIOOBEs stackless.
  `create_stackless_exception_object` demanded an INITIALISED class, and the full
  build (`create_exception_object`) never runs `<clinit>`, so a VM-minted
  AIOOBE/AE stays `UNINITIALIZED` forever unless the program itself says `new`.
  That is why the integrator saw "no difference". The same probe with one
  `new ArrayIndexOutOfBoundsException("x")` in `main`: 148 984 stackless.
* **Full construction is ~1.8 us** of the ~3.2 us (constructor shadow dispatch,
  native-registry lookups, by-name field writes, stack capture, trace-store insert).
* **The rest (~1.4 us) is the OSR round trip**, for `local` and `calleeIdx`:
  every caught exception is an `x64 exceptional frame` stash, `OSR exception-exit
  TRANSFER` to the interpreter handler, then a back-edge `try_osr` ->
  `compile_osr_artifact` (string/Arc allocs, native-registry and gate lookups,
  `get_osr`) -> trampoline re-entry. 198 502 OSR entries for 198 500 exceptions.
* **`callee` on `ExcLoop`** is a different path: `div` is on the optimizing tier,
  whose zero-divisor `Guard` deopts (`TransferToInterpreter`, `Reinterpret`,
  uncharged) and is precise-resumed by the helper: 299 552 deopts for 298 000
  throws. Nothing in the policy ever changes that.

### Interleaved A/B (w9b, `ExcLoop 200000`, last round, ns per exception)

`ExcLoopInit` is `ExcLoop` plus `new ArrayIndexOutOfBoundsException("x")` and
`new ArithmeticException("y")` in `main`, i.e. what the fixed door sees without
the program's help:

| run | flag | local | callee | calleeIdx | pre |
|---|---|---:|---:|---:|---:|
| ExcLoop | 0 | 3081 / 3137 | 4047 / 4128 | 3341 / 3445 | 96 / 95 |
| ExcLoop | 1 | 3129 / 3204 | 4127 / 4162 | 3416 / 3378 | 98 / 98 |
| ExcLoopInit | 0 | 3168 / 3203 | 3970 / 4231 | 3358 / 3359 | 101 / 100 |
| ExcLoopInit | 1 | **1372 / 1370** | **1553 / 1551** | **1590 / 1583** | 108 / 103 |

HotSpot on the same machine (integrator, w9): 396-434 / 690-728 / 396-400 / 27-59.

### Landed

1. **Fast-throw door fixed** (`../../../vm/src/runtime/exceptions.rs`,
   `create_stackless_exception_object` + new `initialisation_is_unobservable` /
   `ClassInitView`). A class is accepted when it is initialised, OR every class
   from it up to its nearest initialised ancestor is untouched (not in progress,
   not erroneous) and declares no `<clinit>` -- initialising such a class runs no
   Java, so skipping it is unobservable, and a later `new` still initialises it.
   That is every `FastThrowKind` class under an initialised `Throwable`. Expected
   effect with the flag ON: the `ExcLoopInit` row above, for `ExcLoop` itself.
   Test: `stackless_door_accepts_a_clinit_free_chain_under_an_initialised_ancestor`.
2. **Hot division guard re-tier** (`../../../vm/src/jit/helpers.rs`,
   `retier_hot_division_guard_trap`, called from `try_resume_trapped_callee`).
   After 16 precise-resumed `TransferToInterpreter` traps AT an
   `idiv`/`ldiv`/`irem`/`lrem` bci of one method (per thread), apply the existing
   site-trap policy once (`claim_site_trap_decision`): ban the IR tier for the
   method and `deoptimize(SpeculationFailed)`, which evicts and recompiles it
   single-pass, whose zero check throws directly (`jit_throw_arithmetic`).
   Default ON, kill switch `CRATONVM_JIT_DIV_GUARD_RETIER=0`. Expected: `callee`
   drops from ~4.0 to the single-pass figure (~3.2 us flag off, ~1.55 flag on) once
   the caller is no longer running a body with the old `CALL` baked in.
   Tests: `r9w10_exc10_division_guard_retier::*`.

Neither is in the w9b binary; both expectations come from the proxies above.

### Still open

* **The OSR round trip per caught exception** (~1.4 us, the floor under
  `local`/`calleeIdx` once construction is gone): the handler runs in the
  interpreter and the loop re-enters OSR through the full `compile_osr_artifact`
  gate chain every time. Lives in `jit_bridge.rs` (cross-lane request to `call10`
  in `NOTES-w10-exc10.md`) and, for the real fix -- running the handler in the
  compiled body -- in the JIT.
* **The IR zero-divisor guard should throw, not deopt** (`../../../jit/src/ir_lower.rs`).
  The re-tier above is the policy-side mitigation.
* **The flag stays default OFF**: ON is `-XX:+OmitStackTraceInFastThrow`
  (HotSpot's default) and 2.3x on these rows, but it breaks this VM's
  "hot throw reads the same as cold" rule
  (`../../../vm/tests/jit_npe_message_hot_equals_cold.rs`). A product decision.

## Status after wave 10b (wire10b)

exc10's two remaining cross-lane requests were reviewed and NOT landed; both stay open as written
in `../../internal/jit-review-r9/NOTES-w10-exc10.md`:

* Request 2 (OSR re-entry after an exception-exit, `jit_bridge.rs`): the note is a design
  suggestion (memoise the validated artifact per `(method, entry_pc)` keyed by the JIT cache
  generation), not an edit. A memo that skips `is_osr_denied`, the de-spec staleness check wave 10
  (call10) just added before the RBC.2 reuse, and the eviction / deopt invalidations needs its own
  invalidation design and a measurement; it is not a safe blind edit.
* Request 3 (IR div-zero `Guard` throws instead of deopting, `ir_lower.rs`): no concrete edit was
  given, and it changes the optimizing tier's exception contract (a throw point with a pending
  exception and handler routing instead of a precise resume). exc10's division-guard re-tier
  already caps the repeated-deopt cost. Left for the IR-lowering owner.

## Integrator measurement after wave 10 (w10 binary, 2026-09-19)

`ExcLoop 200000`, per caught exception, interleaved with w9b, 3 reps (second round of each run):

| arm | local | callee | calleeIdx |
|---|---:|---:|---:|
| w9b | 3.2-4.1 us | 4.3-5.1 us | 3.4-4.2 us |
| w10 (defaults) | 3.3 us | 2.9 us | 3.5-3.8 us |
| w10, `CRATONVM_JIT_DIV_GUARD_RETIER=0` | 3.3 us | 4.4 us | 3.6 us |
| w10, `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1` | 1.5 us | 1.7-2.9 us | 1.7 us |

The division-guard re-tier works (callee 4.4 -> 2.9 us). The fast-throw door now engages
(about 2x), but the flag stays default OFF because of the product rule in
`../../../vm/tests/jit_npe_message_hot_equals_cold.rs`. That is the owner's call.

Confirmed on w10: with `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1` the regression vector
`RExceptions` fails, because hot implicit exceptions lose their stack trace. It passes with the
flag off and with `CRATONVM_JIT_RETIRE_CELL=1`. Turning the flag on by default means changing that
vector's contract first.

## Status after wave 11 (lane `osr11`)

### Where the round trip goes (w10, sampled)

`ExcLocal` (the `local` kernel alone, 60 x 200 000 iterations, scratch probe
`...\scratchpad\w11\src\ExcLocal.java`) with `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1`, so
construction is out of the way, `diag/sampler.py` on the main thread, top self frames:
`try_osr` 9.7 %, `mi_malloc_aligned` 5.2 %, `compile_osr_artifact` 4.1 %,
`route_osr_exception_out_of_artifact` 3.4 %, `HashMap<str>::contains_key` 3.4 %,
`deopt::resolve_frame_state_machine` 3.3 %, `exceptions::instance_field_index_by_name` 2.6 %,
`conservative_roots::active_compiled_frames` 2.5 %, `JitCache::get_osr` 2.2 %,
`osr_trampoline` 2.1 %, `push_entry_full` 2.0 %, `try_fast_throw` 1.8 %, JVMTI
`is_event_enabled` + `manager_for_vm` 2.5 %, `compilation_epoch_for` 1.2 %. The door itself
(`compile_osr_artifact` and what it calls: three `String` + three `Arc<str>` allocations, the
optimizing tier's SipHash + mutex-guarded refusal probe, the synchronized scan under the
class-manager lock, the native-registry probes, the OSR denial probe, `get_osr`, the de-spec
staleness check's `format!`) is roughly a fifth of the per-exception time -- the part asked for.

### Landed: the OSR re-entry memo (`../../../vm/src/runtime/interpreter/jit_bridge.rs`)

`OsrReentryMemo` / `osr_reentry_stamp` / `osr_reentry_memo_get` / `osr_reentry_memo_put`, wired
into `try_osr`: one entry per thread remembering the single-pass artifact the door last answered
with, keyed by (VM, the frame's bytecode identity, class id, method name + descriptor, pc) and
stamped with `JitCache::generation()` and the deopt ledger's `total_deopts()`, both read BEFORE the
lookup. A hit skips the optimizing-tier probe and `compile_osr_artifact` entirely. Why a hit is the
answer the full trip would give (the doc on `OsrReentryMemo` has the whole argument): every input
of that answer is either static for a running method (disable/bisect levers, `ACC_SYNCHRONIZED`,
native shadow, gpu gate), or the cache contents (every `put`/`put_osr`/removal bumps the
generation; the artifact's `retired` flag is checked too), or the de-spec / epoch / not-compilable
state (all moved only inside `deoptimize`, which records a deopt first), or the OSR denial set
(re-asked on each hit). The artifact is held as a `Weak`. Kill switch
`CRATONVM_JIT_OSR_REENTRY_MEMO=0` (default ON). `CRATONVM_DBG_JITC` prints
`OSR-reuse(memo)` for a hit.

Test: `r9w11_osr11_tests::the_reentry_memo_hits_only_while_its_stamps_hold`.

Not measured (the binary predates it). Expected: `local` / `calleeIdx` lose roughly the door's
share (~0.2-0.3 us of ~1.7 us with the fast-throw flag on; the same absolute amount with it off).
Measure `ExcLoop 200000` interleaved against w10 and with `CRATONVM_JIT_OSR_REENTRY_MEMO=0` as
the in-binary control; `CRATONVM_DBG_JITC=1` on `ExcLoop 20000` should show `OSR-reuse(memo)`
for almost every entry.

### Declined: keeping the handler in compiled code

Running the `catch` inside the OSR body means the single-pass backend dispatching a throw to an
in-body handler (exception-table lowering in `jit/src/x64/*`, a handler entry with the operand
stack reset and the throwable in a register, the precise-frame contract for what the handler
reads). None of that is in this lane's files, and it is a codegen feature rather than an edit. The
rest of the per-exception cost is also outside them: the exceptional-frame reconstruction
(`deopt::resolve_frame_state_machine`), the JIT entry guard (`conservative_roots`), exception
construction (`exceptions.rs`), and the per-catch JVMTI probe (`jvmti_events.rs`).

### Still open

* The handler runs in the interpreter: one exception-exit TRANSFER + one OSR entry per caught
  exception (now without the door's gate chain).
* `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` stays default OFF (product rule).
* The IR zero-divisor guard (lane `irl11`).

## Status after wave 11 (irl11): the IR zero-divisor guard -- deferred as a performance/design choice

Only this item is lane `irl11`'s ("the IR zero-divisor guard should throw, not deopt",
`../../../jit/src/ir_lower.rs`). It remains **not implemented**, but the routing defect that
previously made the direct-throw protocol unsafe was fixed on 2026-09-20; see
`../../internal/fixed-bugs/compiled-implicit-exception-outside-a-try-is-caught-by-an-unrelated-handler-FIXED-20260920.md`.

* The former direct-throw protocol routed with `JitThrowPc::Unknown`, so an unrelated typed
  handler could catch a divide by zero outside every `try`. The shared exception edges now stamp
  their original BCI and all three JIT-return drains use that location, so the dispatcher returns
  `OutsideAllRanges` and propagates correctly.
* The optimizing tier still deopts at the division's BCI and re-executes `idiv`, which is correct
  and remains the implemented path. Moving to a direct throw is an independent optimization:
  `ir_lower.rs` needs an IR-visible arithmetic helper and a reliable guard BCI. It does not need
  to grow exception-table knowledge merely to avoid the retired routing bug.

If pursued, the IR guard's pad should stamp the guard BCI before calling
`jit_throw_arithmetic` and exiting through the exception epilogue. The direct throw then receives
the same position-aware routing as the single-pass path. The division-guard re-tier remains an
independent policy choice; historical investigation notes are in
`../../internal/jit-review-r9/NOTES-w11-irl11.md`.

## Integrator measurement after wave 11 (w11 binary, 2026-09-19)

`ExcLoop 200000`, rounds 2-3, interleaved with w10, 3 reps:

| arm | local | callee | calleeIdx |
|---|---:|---:|---:|
| w10 | 3.26-3.52 us | 2.88-3.15 us | 3.50-3.74 us |
| w11 | 3.00-3.11 us | 3.11-3.35 us | 3.21-3.37 us |
| w11, `CRATONVM_JIT_OSR_REENTRY_MEMO=0` | 3.25-3.41 us | 3.47-3.60 us | 3.49-3.61 us |

The re-entry memo saves about 0.25-0.3 us per caught exception. What remains is the
interpreter handler round trip itself. The former single-pass routing correctness defect is
retired; it no longer blocks evaluating a direct-throw IR implementation.
