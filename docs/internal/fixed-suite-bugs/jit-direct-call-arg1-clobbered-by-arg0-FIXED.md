# A raw JIT-to-JIT direct call clobbered arg1 with arg0

**Status:** FIXED 2026-08-01 (`fix/ws-serverdeadlock-hang-20260801`).
**Severity:** high — silent wrong answers from any two-argument static/special
callee reached over the direct edge, with no crash and no diagnostic.
**HotSpot:** correct in 8.0e9 calls.
Found while root-causing the Tomcat `TestWsRemoteEndpointImplServerDeadlock`
close delay (`docs/known-issues/tomcat/wsremoteendpoint-server-close-never-completes.md`).

## Symptom

A callee reached over the raw JIT-to-JIT direct edge received its **first
argument in both parameter slots**, so every comparison inside it behaved as if
the two operands were equal.

```java
private static boolean ge(int c, int s) { return c >= s; }
private static boolean f()              { return ge(src.get(), 0); }
```

`src` pinned to `-536870912`, so `f()` must always be `false`. Before the fix it
was `true` for 1,864,015 of 1,865,000 calls; HotSpot, 0 of 8,015,620,000. Putting
the argument in a local first (`int c = src.get(); return ge(c, 0);`) was correct
— that shape lets the inliner take the call, so no raw edge is emitted.

Measured with `a = -536870912`, `b = 0` (or `-1`), the four comparisons were
exactly `cmp a, a`:

| callee | truth | before the fix |
|---|---|---|
| `a < b`  | true  | **false** |
| `a > b`  | false | false |
| `a == b` | false | **true** |
| `a >= b` (b = -1) | false | **true** |

## Root cause

`pop_stack` reclaims a top-of-stack `Frame` slot (`next_spill_offset -= 8`) but
**still hands the slot back**, and the returned `StackSlot::Frame`s stay live
until `emit_stack_arg_setup` marshals them into the entry ABI. The direct-call
paths popped the arguments and then called `reserve_spill_slots(arg_slots.len())`
for the cold exception-table argument-service copy — which handed back **the very
slots the arguments still occupied**. The copy writes `base + (len-1-i)` while
reading `arg_slots[i]`, i.e. a reversing copy into itself:

```asm
51f:  mov [rbp-0x30],rax   ; push src.get()      (slot A)
525:  mov [rbp-0x38],rax   ; push iconst_0       (slot B)
529:  mov r11,[rbp-0x30]   ; r11 = A
52d:  mov [rbp-0x38],r11   ; <-- B := A   the defect
535:  mov rdi,r11          ; ABI arg0 = A   correct
538:  mov rsi,[rbp-0x38]   ; ABI arg1 = B = A   wrong
```

(The reload of slot B for the second copy iteration was elided by store-to-load
forwarding, which is why only three instructions appear.)

Both compiled bodies of the callee were dumped and disassembled and both were
correct, as were `emit_stack_arg_setup`'s `arg_slots`. The service range also had
to outlive the CALL for `emit_inline_callee_deopt_check`, which the aliased range
could not — so the cold deopt path was reading clobbered arguments too.

## Fix

Two sites in `jit/src/x64.rs` (the `invokestatic` direct call and the
`invokespecial`/virtual direct call): remember the operand-stack top **before**
the pops and place the service reservation above it.

```rust
let args_frame_top = self.next_spill_offset;   // before the pop loop
...
if self.next_spill_offset < args_frame_top {
    self.next_spill_offset = args_frame_top;
}
let base = self.reserve_spill_slots(arg_slots.len())?;
```

The spill reserve was exactly `max_stack * 8` with no headroom, so the service
range can now need slots past the limit — `checked_spill_range_end` fails closed
and would drop the whole compile. `spill_size` therefore gains
`min(max_stack, 16)` slots of headroom: a call site's arguments are themselves on
the operand stack, so `max_stack` extra slots is always sufficient, and the cap
stops a deep-stack method doubling its frame for a copy that can never be that
wide.

After the fix the same call site emits:

```asm
529:  mov r11,[rbp-0x30]   ; read arg0
52d:  mov [rbp-0x48],r11   ; service range now ABOVE the args
531:  mov r11,[rbp-0x38]   ; read arg1 — intact
535:  mov [rbp-0x40],r11
539:  mov rdi,[rbp-0x30]   ; ABI arg0
53d:  mov rsi,[rbp-0x38]   ; ABI arg1
```

## Verification

`TaskQueue.force` is the real Tomcat code that failed; it is public and its only
failure mode is `parent == null || parent.isShutdown()`, so hammering it on a
running executor tests the exact broken predicate with no host-load dependence:

| | `force()` calls | "Executor not running" rejections |
|---|---|---|
| HotSpot | 47,786,000 | 0 |
| CratonVM before | 1,681,000 | **1,680,487** (99.97%) |
| CratonVM after | 194,000 | **0** |

Repro ladder, all `bad=0` after the fix and all firing before it:
`probes/ArgMarshalProbe2.java` (minimal), `Arg1Probe` (which argument + the
comparison matrix), `ShapeMatrixProbe` (which call shapes), `ReturnValueProbe`
(the callee returns a clean 0/1, so it took the wrong branch rather than
returning garbage), `DiagBranchProbe`, `IntCmpProbe`, `ArgMarshalProbe.java`
(constant-source control), `ForceProbe` / `ShapeDiffProbe` (need the Tomcat jar).

No compile-coverage regression: unique JIT-compiled methods over a Tomcat
WebSocket run, three reps each — before 174/140/171, after 175/175/167. (An
earlier 39-vs-34 reading came from a 40-method probe and was noise.)

WebSocket cluster identical before and after: `TestWsPingPongMessages`,
`TestEncodingDecoding`, `TestWsSessionSuspendResume` PASS;
`TestAsyncMessagesPerformance` and `TestWsRemoteEndpointImplClient` FAIL
identically on both (pre-existing).

`cargo test -p cratonvm-jit` could NOT be run: its test build is broken on this
branch independently of this change (`x64.rs:29302` calls
`compile_with_param_slots` with 30 of 32 arguments, E0061). Confirmed pre-existing
by reverting this change and rebuilding the tests — same error.

## Blast radius

`ThreadPoolExecutor.isShutdown()` is `runStateAtLeast(ctl.get(), SHUTDOWN)` —
exactly this shape, with a `ctl` that is always negative while the pool runs. Any
`f(g(), k)` whose callee compares its two arguments was exposed, silently.
