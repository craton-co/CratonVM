# A raw JIT-to-JIT direct call clobbers arg1 with arg0

**Status:** OPEN (root-caused to the emitted bytes; the emitter that writes the
bad store is not yet pinned). **HotSpot:** correct in 8.0e9 calls.
Found 2026-08-01 while root-causing the Tomcat
`TestWsRemoteEndpointImplServerDeadlock` close-delay failure
(`docs/known-issues/tomcat/wsremoteendpoint-server-close-never-completes.md`),
which this defect causes.

## Symptom

A two-int static method called from a compiled caller receives its **first
argument in both parameter slots**. Every comparison inside it therefore behaves
as if the two operands were equal.

```java
private static boolean ge(int c, int s) { return c >= s; }
private static boolean f()              { return ge(src.get(), 0); }
```

`src` is pinned to `-536870912`, so `f()` must always be `false`. On CratonVM it
is `true` for 1,864,015 of 1,865,000 calls; on HotSpot, 0 of 8,015,620,000.

The same predicate written with the argument in a **local** first is correct:

```java
private static boolean f2() { int c = src.get(); return ge(c, 0); }   // correct
```

That is the whole difference. The two shapes call the same `ge`; the second one
lets the inliner take the call, so no raw edge is emitted.

## Why the four comparison results look like "operands are equal"

Measured with `a = -536870912`, `b = 0` (or `-1`), each callee reached through
the failing shape:

| callee | evaluates | truth | CratonVM |
|---|---|---|---|
| `a < b`  | `a < a`  | true  | **false** |
| `a > b`  | `a > a`  | false | false |
| `a == b` | `a == a` | false | **true** |
| `a >= b` (b = -1) | `a >= a` | false | **true** |

All four are exactly what `cmp a, a` produces.

## The emitted bytes

`CRATONVM_DBG_DUMP_JIT=f_lt`, caller `f_lt() { return ltZero(src.get(), 0); }`,
single-pass backend, `objdump -M intel`:

```asm
50a:  call   rax                        ; AtomicInteger.get()  -> RAX
516:  cmp    rax,r10                    ; exception sentinel
51f:  mov    QWORD PTR [rbp-0x30],rax   ; push get() result      (stack slot A)
523:  xor    eax,eax
525:  mov    QWORD PTR [rbp-0x38],rax   ; push iconst_0          (stack slot B)
529:  mov    r11,QWORD PTR [rbp-0x30]   ; r11 = A
52d:  mov    QWORD PTR [rbp-0x38],r11   ; <-- B := A   ***the defect***
531:  mov    QWORD PTR [rbp-0x30],r11   ;     A := A   (no-op)
535:  mov    rdi,r11                    ; ABI arg0 = A            (correct)
538:  mov    rsi,QWORD PTR [rbp-0x38]   ; ABI arg1 = B = A        (wrong)
...
5a9:  call   0x2c000                    ; raw JIT-to-JIT CALL
```

The store at `0x52d` overwrites the operand-stack slot holding the second
argument with the first argument's value, before `0x538` reads it. `arg_slots`
themselves are right — `emit_stack_arg_setup` loads `ARG_REGS[0]` from
`Scratch(R11)` and `ARG_REGS[1]` from `Frame(-0x38)`, both as intended. The
damage is done by the three-instruction block at `0x529`–`0x531`, which reads one
stack slot and writes it to two.

Both compiled bodies of the callee were dumped and disassembled and **both are
correct** (single-pass: `cmp r13d,r12d ; jge`; IR tier: `cmp eax,ecx ; setge al ;
movzx eax,al` with correct phi copies on both edges). The callee is not at fault.

## Scope

- Needs the callee to be reached over the **raw JIT-to-JIT direct edge**. Any of
  these three switches removes the failure completely:
  `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`, `CRATONVM_JIT_IR_DIRECT_CALL=0`,
  `--nojit`.
- NOT the safepoint machinery: `CRATONVM_NO_MOVING_YOUNG=1`,
  `CRATONVM_JIT_MY_SCRATCH_FLUSH=0`, `CRATONVM_JIT_SAFEPOINT_REG_SPILL=0` and
  `CRATONVM_JIT_SAFEPOINT_POLLS=0` all leave it firing.
- NOT `CRATONVM_JIT_SP_TAILCALL=0`, NOT `CRATONVM_JIT_SP_INLINE_IC=0`,
  NOT `CRATONVM_JIT_KERNEL_REG_LOCALS=0`,
  NOT `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0`.
- Needs a **concurrently mutated** argument source. With a constant source the
  same shapes are all correct (`probes/ArgMarshalProbe.java`), presumably because
  the inliner takes the call instead.
- Does NOT need load: 4 mutator + 4 reader threads reproduce it at 99.98%.
- Arguments other than the clobbered one are fine, and a callee that ignores its
  arguments (`return 1;`) is fine — only a callee that *uses* the second int
  argument is affected.

## Reproduction

`probes/ArgMarshalProbe2.java` — `f6` is the failing shape, `f5` the control
(identical call, `int`-returning callee, always correct).

```bash
javac -d . ArgMarshalProbe2.java
cratonvm --java-home <jdk> --Xmx 2g -c . ArgMarshalProbe2 6
```

Expected on a fixed VM: every `bad=0`. Today `f6` reports ~2.8M.

Companion probes: `probes/Arg1Probe.java` (which argument, and the comparison
matrix above), `probes/ShapeMatrixProbe.java` (which call shapes are affected),
`probes/ReturnValueProbe.java` (proves the callee returns a clean 0/1, i.e. it
takes the wrong branch rather than returning garbage).

## Why this matters beyond the WebSocket test

`ThreadPoolExecutor.isShutdown()` is `runStateAtLeast(ctl.get(), SHUTDOWN)` — the
exact failing shape, with a `ctl` that is always negative while the pool runs. It
answered "shut down" for a running Tomcat connector pool, which made
`TaskQueue.force` reject a socket-processing task, which made Tomcat close a live
WebSocket connection. Any `f(g(), k)` on a hot path where the callee compares its
arguments is exposed the same way.
