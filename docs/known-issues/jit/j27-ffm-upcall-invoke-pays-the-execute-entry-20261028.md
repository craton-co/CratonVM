# An FFM up-call's Java call costs more than the rest of the up-call: `interpreter::execute`'s entry

> **j27 wave 8 (lane W8UPC, 2026-10-28): PARTLY (item 3, opt-in), PENDING HOST VERIFICATION.** Item 3 taken
> the cheap way that needs no new `NativeContext` method and no prediction: the key is still the first
> argument's class, but `static_first_arg_key` validates the header once and reads kind and class through
> the `_validated` twins (`vm/src/runtime/native_callee_memo.rs` `static_first_object_class_validated_once`),
> under `CRATONVM_NATIVE_CALLBACK_MEMO_STATIC_KEY_VALIDATE_ONCE=1` (opt-in, `VmConfig::static_memo_key_validate_once`,
> read once per VM). Re-checked the other per-call steps of `invoke_static_settling_door` for an up-call: the
> served-raise door note is screened off for an application target (`door_in_flight_screens`: `java/` /
> `jdk/` only), and `forward_boundary_args` allocates only on a move; neither is worth a switch. Items 1 and 2
> unchanged (interpreter area; analysis in the wave 6 block below). The stub-known key (a minted segment class
> handed in by the caller) stays a follow-up: price this switch first with the recipe on
> [`ffm-upcall-per-call-costs-microseconds-20261027`](ffm-upcall-per-call-costs-microseconds-20261027.md)'s wave 8
> block; if the key is still visible in a profile with `=1`, the caller-supplied key is the next step.
> Found on the way (not built): the by-name static tail (`vm_exec.rs`, the `note_plain_static_tail` call
> after `note_plain_bytecode_tail`) computes `static_first_arg_key` on EVERY static by-name tail, armed
> witness or not, and `note_plain_static_tail` then returns at once unless a static witness is armed. A
> lazy key (a closure asked only once the armed witness matches owner, strings and depths) would drop
> both header validations from every unarmed static tail. Behaviour-identical; it is the same per-VM
> switch's measurement (the same key function), so take it after this one is priced.

> **j27 wave 6 (lane T1LOOPSTATIC, 2026-10-28): NOT FIXED (analysis only, no code), PENDING HOST
> MEASUREMENT.** No switch was added or changed.
>
> **1. Why the two execute switches are off, and whether the reason still holds.**
> * `CRATONVM_EXECUTE_FEWER_COPIES` (`VmConfig::execute_fewer_copies`, `vm/src/config.rs`; w83 lane L3,
>   page `docs/internal/fixed-bugs/interpreter-i81-L3-proposal-name-the-reflective-calls-gap-and-reentry-KEPT-OFF-20261019.md`):
>   KEPT-OFF at the wave-91 triage and in `docs/internal/reviews/interpreter-flag-triage-i63-20261006.md`
>   as "mixed / inside the noise across modes" on the reflective rows (`L3W83ReflectCallBench`,
>   `L3W81ReflectGateBench`). It has no parity cost: an empty exception table becomes the shared
>   `Arc::default()`, and the gate-pass memo key is built once ("Behaviour identical either way").
>   **The reason does not carry over to this row:** those benches measured a door that is off the
>   default path since `CRATONVM_JDK_REFLECTIVE_INVOKE`, while the up-call reaches `execute` once per
>   call with an empty table (the comparator). FFMUP's single batch (2,124 -> 1,936 ns, -9%) is inside
>   its own ±20% band, so it decides nothing. It is a fair candidate for a re-A/B, not yet a flip.
> * `CRATONVM_EXECUTE_LEAN_REENTRY` (`VmConfig::execute_lean_reentry`; i114 lane L7, page
>   `docs/internal/fixed-bugs/interpreter-i114-L7-native-reentry-cost-split-KEPT-OFF-20261224.md`):
>   **failed its rule** in the i6 switch triage
>   (`docs/internal/reviews/interpreter-switch-triage-i6-20261018.md`, table: `L7W114NativeReentryBench`
>   `--nojit` 0.92-1.04, JIT `reflect-virtual` **1.16**, "off (losing arm)"). It also has a parity caveat:
>   a deferred source-file read sees a redefinition made between the guard and the frame push, while
>   HotSpot keeps one `SourceFile` per class version. Every interpreted re-entry also pays one more
>   class-manager `read_recursive`. Those reasons still hold, so it stays OFF (user rule: a failed
>   switch stays off). It also implies the `FEWER_COPIES` cuts, which is why "both on" does not add up.
> * Recipe to decide `FEWER_COPIES`: same binary, pinned (`start /affinity FFFF`), interleaved, 7 reps,
>   medians, both modes, JIT and `--nojit`:
>   `R22FfmUpcallCost 20000 target`, `J27FfmUpcallRows -Dj27.time=1`, `L7W114NativeReentryBench`,
>   `L3W83ReflectCallBench`, A = default against B = `CRATONVM_EXECUTE_FEWER_COPIES=1`. Every `fact` and
>   checksum must be SAME. Flip it default ON (as a `WARNING:` commit) only if the up-call row wins
>   outside the band in both modes and no row of the other three benches loses.
>
> **2. Direct compiled entry from `upcall_dispatch`: not built, because there is no door to reuse.**
> * Every native-to-Java path ends in `interpreter::execute`. This one does:
>   `panama_upcall.rs` `upcall_dispatch` -> `NativeContext::invoke_static_settling` ->
>   `vm_exec.rs` `invoke_static_settling_door` -> `invoke_static_settling`, whose settled memo arm calls
>   `execute(declaring, ..)` and whose other arms go through `invoke_static_on_class_shared` /
>   `invoke_static_on_owner`. JNI does too: `vm/src/native/jni.rs` `jni_call_static`, which receives an
>   exact `(declaring class, method index)` jmethodID, still calls `invoke_on_class_shared` by name.
> * The `jit_bridge` doors (`execute_jit_call`, `execute_jit_call_decoded`, `execute_jit_call_oneshot`)
>   all pop their arguments off the CALLER'S interpreter operand stack (`frame_idx`), and a native
>   caller has none.
> * The only native-to-compiled code is `execute`'s own compiled tail, `interpreter.rs` around
>   `jit_bridge::run_jit_body_raw(_promised)`. Around the raw call it carries: the depth and native-stack
>   ceilings, the JVMTI/JDWP interp-only gates, not-entrant and seal checks, the synchronized monitor,
>   the weak-argument promise and `InvokeArgsRootGuard` pins, the `RawJitBodyOutcome` arms (the
>   `Returned` sentinel rule for `J`/`D`/`V`, `Deopt` and the frameless re-run with
>   `note_door_rerun_of_method` / `unsound_rerun_refused`, `Stashed` frame resume, `Throw` with its
>   handler-frame route), the census stamps and the tier counters.
> * A second copy of that tail for one caller is exactly what
>   `docs/internal/fixed-bugs/interpreter-L7-legacy-first-call-jit-door-duplicates-run-jit-body-FIXED-20260924.md`
>   removed. Writing one without a build or a deopt/throw battery would be unsound, so this lane did not.
> * The sound shape is one per-method entry record that `execute` itself uses, so the up-call does not
>   go around `execute`. That record would hold the gate verdicts, the code view, the exception-table
>   and source-file `Arc`s and the published body's identity plus the JIT-cache generation. It would be
>   keyed by `(ClassId, method index)` and handed over by a caller that already knows the method (the
>   up-call's `DirectStatic`, a jmethodID). `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME`'s
>   `execute_reentry_record` / `JitRealm::execute_reentry_records` and `CRATONVM_NATIVE_OWN_BODY_HINT`'s
>   `set_execute_method_hint` are the two partial versions already in the tree, both default off. The
>   missing parts are the JIT gate block (skip set, gate-pass memo, `JitCache::get_for_reentry`:
>   58 + 43 + 69 of the 999 samples) and withdrawing the verdict at every seal site (see the i81 page's
>   "JIT gate block" note). That is an interpreter-round design with its own pages, not a JIT-lane patch.
>
> **3. `static_first_arg_key` (105 samples): not changed.** Its first-argument class exists because
> `invoke_on_class_shared_inner`'s argument-keyed arms (array `clone`, `try_lambda_dispatch` for a
> lambda proxy) read `args[0]`. The memo is a witness, not a prediction (`native_callee_memo.rs`
> module docs), so dropping the key for a static target would be a prediction. An up-call could instead
> pass a key it knows statically: `STATIC_FIRST_NOT_AN_OBJECT` for a primitive first parameter (already
> cheap), or the minted segment class for an address. That class is the same for every call only on
> the `JdkSegmentMint` route (`mint_address_args_batched`), not on the `--compatible` or unbatched
> routes. It would need a new `NativeContext` method. Measure its share first by doubling it (the
> memory recipe "measure a cost by doubling it").

| | |
|---|---|
| **Status** | **OPEN** (perf). Filed 2026-10-28 by j27 wave 5, lane FFMUP, while working [`ffm-upcall-per-call-costs-microseconds-20261027`](ffm-upcall-per-call-costs-microseconds-20261027.md). |
| **Area** | `vm/src/vm/vm_exec.rs` `invoke_static_settling` (the native-callback memo arm), `vm/src/runtime/interpreter.rs` `execute` (the entry before a compiled body runs). Not the FFM lane's files. |
| **Probe** | `tools/probes/jit/r22/R22FfmUpcallCost.java 20000 target`; `tools/probes/jit/j27/J27FfmUpcallRows.java -Dj27.time=1`. |

## Evidence (Windows, P-cores, `--jdk-only`, 2026-10-28 tree)

Cost by removal and doubling inside one binary (scratch knobs, since removed; medians of 5,
interleaved, ns per up-call of the `target` row, which is about 2,100-2,700 ns in total):

| Arm | ns per up-call |
|---|---|
| base | 2,691 |
| each address segment minted twice | 3,072 (+381 for 2 mints: about 190 ns per segment) |
| the target not invoked (comparator answers 0) | about 1,150 |

So about 1.5 µs of an up-call is the Java call `upcall_dispatch` makes through
`NativeContext::invoke_static_settling`. HotSpot's whole up-call is 45-105 ns on this machine.

The stack sampler (`StackSampler.cs` StackWalk64, kept in the lane's bin dir), folded under
`invoke_static_settling` (999 samples):

| Samples | What |
|---|---|
| 240 + 29 | `interpreter::execute` copying the callee's exception table (`Arc::from_iter_exact<ExceptionTableEntry>`, `execute_exception_table`) and source file (`Arc::copy_from_slice<u8>`) per call, both default-off fixes: `CRATONVM_EXECUTE_FEWER_COPIES`, `CRATONVM_EXECUTE_LEAN_REENTRY` |
| 105 | `native_callee_memo::static_first_arg_key` (`kind_of`, `class_id_of`, `is_lambda_proxy_class` of the first argument) |
| 72 | `str::starts_with` / `memcmp` in the entry |
| 58 + 43 + 69 | `JitCache::get_for_reentry`, `env_cache::jit_gate_pass_memo`, `HashMap` probes |
| 157 | the compiled comparator body itself (`run_jit_body_raw`) |

An A/B of the two existing execute switches (`=1`, medians of 5, noisy: ±20% between batches):
base 2,124, `CRATONVM_EXECUTE_FEWER_COPIES=1` 1,936, `CRATONVM_EXECUTE_LEAN_REENTRY=1` 1,859,
both 2,076.

## What would fix it

1. Decide the two default-off execute switches (interpreter area): with the comparator's empty
   exception table, `FEWER_COPIES` removes the per-call `Arc` copy that is a quarter of the
   up-call's Java call.
2. A native-to-compiled entry for a settled static target that skips `execute`'s by-name
   re-entry probes: the up-call stub knows its member once (`DirectStatic`), so the VM could
   hand it a resolved entry (method identity + JIT cache generation) to call directly.
3. `static_first_arg_key` is asked for every settled static callback; a static target's memo
   key does not need the first argument's class when the target is not an interface/lambda
   proxy dispatch.
