> **j27 wave 8 (lane W8UPC, 2026-10-28): PARTLY (one more per-call cost cut, opt-in), REVIEWED, PENDING HOST
> VERIFICATION.** **Cut:** the next named cost in the up-call's Java call is the static callback memo's key,
> `native_callee_memo::static_first_arg_key` (105 of the 999 samples under `invoke_static_settling`, see
> [`j27-ffm-upcall-invoke-pays-the-execute-entry`](j27-ffm-upcall-invoke-pays-the-execute-entry-20261028.md)):
> its `kind_of` and `class_id_of` each run the full conservative header validation on the first argument (the
> minted segment). Under `CRATONVM_NATIVE_CALLBACK_MEMO_STATIC_KEY_VALIDATE_ONCE=1` (opt-in, per VM:
> `VmConfig::static_memo_key_validate_once`) it validates once (`is_object_start`) and reads both through the
> `_validated` twins (new `static_first_object_class_validated_once`, `vm/src/runtime/native_callee_memo.rs`),
> the pattern of `opcodes::validated_receiver_header`. Same key for every live object; an address the validator
> refuses is no longer memoised under class 0. It serves every settled `REF_invokeStatic` door (method handles
> too), so it is opt-in until measured. Tests (vm): `runtime::native_callee_memo::j27_w8upc_static_key_validate_once_tests`.
> Probe `tools/probes/jit/j27/J27W8UpcKeyRows.java` (HotSpot 25: `fact tag-seq 1234012340`, `fact tag-sum 20000`,
> `fact twice-sum 99990000`, `fact copy-last false/true/5001`, `fact lambda-runs 3000`). **Reviewed** wave 5's
> `mint_address_args_batched` / `stub_segment_mint`: error paths release every pin, the segments are unpinned
> before the call exactly as the per-argument loop did (`forward_boundary_args` repairs a move), the cached mint
> is per stub and a stub belongs to one VM; no defect found. **Verify (host):** facts of `J27W8UpcKeyRows`,
> `J27FfmUpcallRows`, `R22FfmUpcallCost` checksums equal HotSpot in both modes, default and `=1`, JIT and
> `--nojit`; timing: same binary, pinned, interleaved, 7 reps, medians, `R22FfmUpcallCost 20000 target` and
> `J27FfmUpcallRows -Dj27.time=1`, A = default, B = `=1`. Flip default ON (WARNING commit) only if B wins in
> both modes outside the band and a settled-handle row (`L3W64DoorKeyBench` `handle-static-jdk`) does not lose.

> **j27 wave 5 (lane FFMUP, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Built and measured
> on Windows (P-cores, interleaved, medians). **Where an up-call goes now** (`target` row, about
> 2.0-2.7 µs; measured by removal and doubling with scratch knobs, since removed): the Java call
> through `invoke_static_settling` is about 1.5 µs (55%). Most of that is `interpreter::execute`'s
> entry, not the compiled comparator. Filed as
> [`j27-ffm-upcall-invoke-pays-the-execute-entry`](j27-ffm-upcall-invoke-pays-the-execute-entry-20261028.md)
> (VM/interpreter area). The two argument segments cost about 190 ns each. The region leave and
> re-entry are lost in the noise. **Done (item 2):** `panama_upcall::mint_address_args_batched` mints
> every address argument of a stub with no by-value group parameter through ONE
> `panama::JdkSegmentMint`. That mint is resolved once per stub (`UpcallStubData::mint`,
> `stub_segment_mint`; a refusal is not kept). All segments are allocated first, with the session
> and each segment pinned, then initialised together with one `GLOBAL_SESSION` read. The decoded
> arguments go to a stack buffer, not a `Vec`. `panama.rs` `pe_jdk_native_segment_with` is now
> `JdkSegmentMint::{alloc, global, init}`, the same stores. The switch is
> `CRATONVM_FFM_UPCALL_MINT_BATCH` (perf, default ON, `=0` restores the per-argument loop; WARNING
> j27-user-flip-20261028 marker). **Timing**, ns per up-call, ON vs `=0`:
>
> | Row | ON | `=0` |
> |---|---|---|
> | `target`, `--jdk-only` (11 reps) | 2,052 | 2,607 |
> | `target`, `--compatible` (9 reps) | 1,980 | 2,211 |
> | `reinterpret`, `--jdk-only` (19 reps, bistable: two states) | 7,694 / 14,083 | 7,801 / 14,146 |
> | `J27FfmUpcallRows -Dj27.time=1`, `--jdk-only` (5 reps) | 2,010 | 2,104 |
>
> HotSpot: 44 ns (`R22FfmUpcallCost`) and 91-108 ns (`J27FfmUpcallRows`). **Facts:**
> `J27FfmUpcallRows` (new) and `J27FfmUpcallThrows` (new; exit 1 with HotSpot's 4-line stderr)
> equal HotSpot in both modes, ON and `=0`. `R22FfmUpcallCost` checksums equal HotSpot's
> (`-893077807541524751`), as does `R22FfmCompiledGc 2`. The `--compatible` runs set
> `CRATONVM_COMPAT_FAST_BOOT_JIT=0`: the tree's known fast-boot defect breaks every
> `printStackTrace` there, with or without this change. `J27FfmUpcallRows` is also SAME under
> Serial, G1 and ZGC. **Item 3:** `CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS=1` does not help.
> Half the asks still take the security walk; filed as
> [`j27-reinterpret-in-an-upcall-takes-the-caller-security-walk`](../../internal/jit/j27-reinterpret-in-an-upcall-takes-the-caller-security-walk-FIXED-20261029.md).
> **`R22FfmCompiledGc`:** on Windows 38-55 s against HotSpot's 2 s, the same with ON and `=0`. Its
> main thread spends 78% in `safepoint_check` under the churn thread's `System.gc()`. Up-calls are
> 3% of that thread. Filed as
> [`j27-r22-ffm-compiled-gc-time-is-safepoints-not-upcalls`](j27-r22-ffm-compiled-gc-time-is-safepoints-not-upcalls-20261028.md).
> **Host verify:** `J27FfmUpcallRows` and `J27FfmUpcallThrows` equal HotSpot in both modes, ON and
> `=0`, under G1 and ZGC. `R22FfmUpcallCost 20000 target`, interleaved, ON <= `=0`. Unit tests:
> `cargo test -p cratonvm-native-builtins --lib -- j27_ffmup_mint_batch_tests panama_upcall`.

> **j27 wave 2 (lane INVOKE, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Item 2, read: the
> argument segment's class, session and slots were already resolved once per VM and thread
> (`panama.rs` `JDK_NATIVE_SEGMENT_SHAPE`, round 19/22), so "a pre-resolved class and layout" is in.
> What was left per mint: two context doors (the three primitives batched, then `scope` through its own
> `set_field`). Now one: `pe_jdk_native_segment_with` stores `scope` in the same `set_fields_typed`
> batch (the batch's reference store runs the same SATB pre-barrier and typed store;
> `CRATONVM_NATIVE_BATCH_REFERENCE_STORES`), switch `CRATONVM_FFM_SEGMENT_MINT_ONE_BATCH` (perf, default
> ON, `=0` kill). It also serves every downcall `ADDRESS` return and native `get(ADDRESS)`. Expected gain
> small (one door of two per segment, two segments per comparator up-call); the allocation itself stays.
> Item 3 NOT done: `Reflection.getCallerClass` already has a two-frame fast answer
> (`reflection_caller_class_fast`, `vm/src/vm/vm_exec.rs`), which is refused while any reflective-call /
> served `Thread.run` record is in force unless `CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS=1`
> (default OFF); the host run should first check with `CRATONVM_DBG_CALLER=1` whether the `reinterpret`
> row's asks print `(fast)` or `(security walk)` -- if the latter, that switch (stack-walker area, not this
> lane's) is the cheap fix; the compile-time answer for an inlined `@CallerSensitive` callee is JIT work.
> Verify: `R22FfmUpcallCost` checksums equal HotSpot's in both modes, default and `=0`; `ns-per-upcall`
> (`target`, `reinterpret`) default <= `=0` (interleaved medians); `R18FfmretShapes globalScope` and the
> FFM battery unchanged.

> **j27 wave 1 (lane FFMVH, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Item 1 done in code: `native-builtins/src/panama_upcall.rs` `create_upcall_stub_pinned` now also gives a stub a `DirectStatic` when its target is a REAL JDK `DirectMethodHandle` of a `REF_invokeStatic` member called at its own type (`upcall_real_direct_static`, read once from the handle's `MemberName` through `lang_invoke::mh_real_direct_member`), so a `--jdk-only` up-call goes straight to `invoke_static_settling` instead of through the `invokeExact` door (`mh_real_direct_before_linker`, the lane customization checks, the real-handle record). While a debugger observes frames such a stub keeps the door (`DirectStatic::real`), as `mh_real_direct_fast` does. Switch `CRATONVM_FFM_UPCALL_DIRECT_STATIC_REAL` (perf, default ON, `=0` kill; needs `CRATONVM_FFM_UPCALL_DIRECT_STATIC`). Items 2 (argument segments minted without the generic allocation) and 3 (`getCallerClass` without a stack walk) are NOT done; the interpreted comparator body (27% of the `target` row) is unaffected. **Verify**: `R22FfmUpcallCost` checksums equal HotSpot's in both modes with the switch on and at `=0`; `ns-per-upcall` of the `target` row in `--jdk-only` drops by roughly the door's share (expect 30-60%; interleave on/off, medians); `R22FfmCompiledGc 2` CPU time compared on/off.

# An FFM up-call reached from a down-call costs microseconds (HotSpot: about 0.1 µs)

| | |
|---|---|
| **Status** | **OPEN** (perf). Filed 2026-10-27 while retiring [`r22-ffm-qsort-upcall-intermittent-nosuchmethoderror`](../../internal/jit/r22-ffm-qsort-upcall-intermittent-nosuchmethoderror-invokewitharguments-FIXED-20261027.md): this cost is why `R22FfmCompiledGc` ran out of time (rc=124) on slow hosts. |
| **Probe** | `tools/probes/jit/r22/R22FfmUpcallCost.java [sorts [reinterpret\|target]]`: `qsort` over 8 ints with a static `(MemorySegment, MemorySegment)int` comparator. The `ns-per-upcall` lines are informational; the checksum lines must equal HotSpot's. |
| **Not a correctness issue** | The checksums equal HotSpot's in both modes. |

## Measured

Linux host 64.177 (16 cores, load 20-40), the 2026-10-27 build, best of 5 reps, ns per up-call:

| Row | HotSpot 25 | `--jdk-only` | `--compatible` |
|---|---|---|---|
| `target`: the descriptor is `ADDRESS.withTargetLayout(JAVA_INT)`; the comparator reads its arguments directly | 133 | 4,936 (37x) | 15,128 (114x) |
| `reinterpret`: plain `ADDRESS`; the comparator calls `a.reinterpret(4)` and `b.reinterpret(4)` | 73 | 18,018 (247x) | 29,833 (409x) |

`R22FfmCompiledGc 2` (about 800,000 up-calls) took 183 s wall and 99 s CPU against HotSpot's 5.4 s and 2.8 s.

## Where it goes (`--jdk-only`, perf `-g`, folded under `panama_upcall::upcall_dispatch`)

**`target` row (the up-call itself):**

| Share | What |
|---|---|
| 74% | `invoke_exact_at_site` → `NativeContext::invoke_virtual(target, "invokeExact", ..)` → the `MethodHandle.invokeExact` native door. |
| — of which | `lang_invoke::adapters::mh_real_direct_before_linker` (the lane customization checks, `lane_check_customized`, 6%) → `var_handles::mh_real_direct_fast` → `invoke_static_settling` → the comparator's body (27% interpreter `execute`, 9% compiled body). |
| 21% | `new_arg_segment`: two `NativeMemorySegmentImpl` mints per up-call (`pe_jdk_native_segment_with`: allocation, `set_fields_typed`, the ZGC object-start checks). |

**`reinterpret` row, on top of that:** `AbstractMemorySegmentImpl.reinterpret(long)` is `@CallerSensitive` and calls `Reflection.ensureNativeAccess(Reflection.getCallerClass(), ..)` on every call. Each one costs:

- `Reflection.getCallerClass()`: a security-walk of the stack (`stack_walker::reflection_get_caller_class_by_security_walk`, `walk_jit_chain_opts`), 26% of the comparator's native time;
- `Class.getClassLoader` / `getModule` (`lang_class::unnamed_module_for_loader`, `get_cached_module_mirror`), 19%;
- `Unsafe.getBooleanVolatile` / `getByteVolatile` (the module's native-access flag), 16%.

HotSpot inlines all three (`getCallerClass` is an intrinsic).

## What would close it

1. **A cheaper `invokeExact` from Rust for a stub whose target is a plain direct static method handle.** In `--jdk-only`, `DirectStatic` is never set, because `mh_plain_static_leaf` refuses a real JDK handle. The up-call could resolve the member once at stub creation, as `DirectStatic` does for the model handles, and call it with `invoke_static_settling`. The `invokeExact` door would then run only for adapted handles.
2. **The two argument segments minted without the generic allocation path** (a pre-resolved class and layout per stub).
3. **`getCallerClass` without a stack walk** where the caller is the frame directly above (the JIT knows it at the call site). That one is shared with every `@CallerSensitive` JDK method, not FFM only.

Target: `target` row within 5x of HotSpot, and `R22FfmCompiledGc 2` under 30 s CPU.

## j27 wave 5 probes: expected HotSpot 25 output

`tools/probes/jit/j27/J27FfmUpcallRows.java` (`java --enable-native-access=ALL-UNNAMED J27FfmUpcallRows`):

```
fact mixed-return 73
fact mixed-seen 4/0/3/true/false/true/true/7/8
fact mixed-warm-return 77
fact mixed-warm-seen 4/0/7/true/false/true/true/7/8
fact gc-return-sum 400
fact gc-kept-after 4/7/true
fact state-return 9
fact state-in-upcall RUNNABLE
fact group-return 750
fact qsort-checksum -4451889473804424278
```

(`-Dj27.time=1` adds `time qsort-ns-per-upcall N`, informational.)

`tools/probes/jit/j27/J27FfmUpcallThrows.java`: exit status 1. Stdout:

```
fact before-call true
fact in-upcall 42/0
```

Stderr:

```
java.lang.IllegalStateException: j27 upcall throws
	at J27FfmUpcallThrows.thrower(J27FfmUpcallThrows.java:16)
	at J27FfmUpcallThrows.main(J27FfmUpcallThrows.java:29)
Unrecoverable uncaught exception encountered. The VM will now exit
```
