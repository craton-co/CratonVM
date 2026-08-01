# `BasicErrorControllerIntegrationTests` is usable as an acceptance gate again

**Status: FIXED 2026-08-01** on `fix/basicerrorcontroller-jit-20260801`, from
dev `b56da0bba1`. Filed 2026-07-31 as "this class is not a usable acceptance
gate right now"; it is one again.

The class failed 23 of 26 tests under JIT with default flags and passed 26/26
under `--nojit`, which made the acceptance line in
`jit/src/lib.rs::direct_jit_callee_calls_enabled()` — "14 consecutive clean
runs, plus a same-binary gate-closed control" — unrunnable. Two independent
defects were behind the report. Both are fixed; the gate has been run and is
recorded at the bottom.

## 1. The JIT-to-JIT handler resume dropped every non-parameter local

**This is the failure.** One-line summary: a compiled callee's exception
handler, resumed through the JIT dispatch helper, got a frame rebuilt from the
callee's incoming arguments and nothing else — so every local the handler (or
the code it falls through into) reads came back null.

### How it presented

```
IllegalStateException: Cannot bind to SpringApplication
  at SpringApplication.bindToSpringApplication(SpringApplication.java:555)
Caused by: BindException: Failed to bind properties under
           'spring.main.allow-bean-definition-overriding' to boolean
Caused by: NullPointerException: Cannot invoke "java.util.Iterator.hasNext()"
           because "<local5>" is null
  at BindConverter.convert(BindConverter.java:108)
```

Every Spring Boot context boot in the class died the same way, so 23 of 26
tests failed. `spring.main.allow-circular-references` appeared instead of
`allow-bean-definition-overriding` in one run — same defect, different property
reached first.

`BindConverter.convert(Object, TypeDescriptor, TypeDescriptor)` is:

```java
private Object convert(Object source, TypeDescriptor sourceType, TypeDescriptor targetType) {
    ConversionException failure = null;
    for (ConversionService delegate : this.delegates) {   // line 108
        try {
            if (delegate.canConvert(sourceType, targetType)) {
                return delegate.convert(source, sourceType, targetType);
            }
        }
        catch (ConversionException ex) {
            if (failure == null && ex instanceof ConversionFailedException) { failure = ex; }
        }
    }
    ...
}
```

Local 5 is the enhanced-for's synthetic iterator. It is assigned before the
`try`, and the `catch` block does not return — it falls through to the loop
back edge, which reloads local 5 and calls `hasNext()` on it.

### Localisation

Every step is a measurement on the same binary (dev `b56da0bba1`, release,
Linux x86-64, real JDK 25, Spring Boot 4.1.0-SNAPSHOT).

| arm | result |
|---|---|
| HotSpot 25 | PASS 26/26 |
| CratonVM, JIT on, default flags | **FAIL 26 tests / 23 failed** |
| `CRATONVM_JIT_DENY=org/springframework/boot/context/properties/bind/` | PASS 26/26 |
| `CRATONVM_JIT_DENY=java/util/` | FAIL 23 — not the JDK collections |
| `CRATONVM_JIT_DENY=bind/BindConverter` | PASS 26/26 |
| `CRATONVM_JIT_DENY=BindConverter.convert` | PASS 26/26 |
| `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES=1` | PASS 26/26 |
| **with the fix, default flags** | **PASS 26/26** |

`CRATONVM_DBG_RBC6=1` then named the mechanism outright:

```
[rbc6-dbg] try_compile_inner: local_handler_reads_unsafe_local=true for
           BindConverter.convert(Object;TypeDescriptor;TypeDescriptor;)Object;
[rbc6-dbg] run_jit_callee_handler BindConverter.convert(...) throw_pc=18446744073709551615 handler_pc=62
```

### Root cause

`local_handler_reads_unsafe_local` answers "could a handler in this method read
a local that a params-only frame reconstruction cannot recover?". It used to
REFUSE to compile such a method. The precise-handler-frame relaxation
(`precise_handler_frames_enabled`, jit/src/lib.rs) stopped refusing them:
they now compile on the promise that every throwing site in a protected range
publishes a reason-9 exceptional frame carrying the real locals.

Two consumers reconstruct the handler frame.
`interpreter::route_jit_signal_exception` — the interpreter-boundary drain —
was updated with that relaxation and consumes the precise frame.
`interpreter::run_jit_callee_handler` — the JIT-to-JIT dispatch resume reached
from `jit::helpers::route_implicit_exc_through_callee` — was not. Its own doc
comment still carried the retired argument:

> Locals are the callee's incoming arguments … sound for exactly the same
> reason: a compiled method whose handler reads a local first assigned inside
> the try never passes the `local_handler_reads_unsafe_local` compile gate.

That sentence stopped being true when the gate stopped refusing. Which of the
two consumers runs depends only on who called the method — the interpreter, or
compiled code through the dispatch helper — so the same method was correct on
one path and silently wrong on the other.

The `throw_pc=18446744073709551615` above (`usize::MAX`) is the same gap seen
from the other side: without the precise frame there is no throw bci either, so
the handler was selected by exception class alone.

### Fix

`vm/src/runtime/interpreter.rs::run_jit_callee_handler`:

1. Consume the precise exceptional frame when it names this method, and use its
   bci as the throw pc and its locals as the handler frame — the same treatment
   `route_jit_signal_exception` already gave it. A frame naming another method
   is put back exactly as found.
2. With no precise frame, ask `cratonvm_jit::handler_reads_non_param_local`
   (a new public form of the compile gate's own predicate) whether the
   params-only frame is good enough. If it is not, refuse — the caller then
   re-runs the callee from its entry, which replays the pre-throw prefix but
   recovers every local by computing it. Silently substituting null for a live
   local is the worse of the two.

Tests: `jit/src/lib.rs`
`handler_falling_through_to_a_loop_back_edge_reads_the_iterator_local` and
`handler_that_returns_does_not_read_a_non_param_local` pin the predicate on the
exact `BindConverter` bytecode shape (a handler whose trailing `goto` reaches a
loop back edge that reloads the iterator local) and on the negative control.

**No end-to-end fixture.** Four shapes were tried in
`vm/tests/resources/cratonvm/JitPreciseHandlerFrame.java` and none of them
reached `run_jit_callee_handler`: a plain static callee routed through
`route_jit_signal_exception` instead (6492 times per run, confirmed with
`CRATONVM_DBG_RBC6=1`), a static delegate was inlined into the caller, and an
interface-typed receiver kept the callee interpreted. A fixture that passes on
the broken binary is worse than no fixture, so none was committed. The witness
for this path is the Spring Boot class itself, and the table above is its
differential.

## 2. The code-buffer overflow flood — and its misattribution

The original report recorded a flood of

```
JIT try_patch_i32: offset out of bounds; marking buffer overflowed offset=4094 len=4094
```

and attributed it to the single-pass backend's
`ExecutableBuffer::new(estimated_size.max(4096))` in `x64.rs`.

**That attribution was wrong**, and it was wrong for a structural reason: the
warning named neither the buffer's capacity nor the size the body needed nor
which of the four sizing heuristics allocated it, so it could only be
attributed by arithmetic on `len` — and `len` freezes at the first dropped
write, which makes it the one number that cannot answer the question. The
warnings now carry `capacity`, `wanted` and a `buffer` tag; measured on one run
of this class:

| source | warnings per run |
|---|---|
| `ir-lower` (the optimizing tier) | **8072** |
| `x64-single-pass` | 10 |

### 2a. The optimizing tier now measures instead of guessing

`ir_lower::lower_inner` sized its buffer as `nodes * 32 + calls * 448 + 1024`.
One number for a call site whose real cost swings by several hundred bytes
depending on which lowering it picks, and widening the PIC's inter-slot branch
from `rel8` to `rel32` (part of `7f1b1f263`) pushed the expensive end past it.
`ExecutableBuffer::emit` then drops the write, sets the sticky `overflowed`
flag, and the method stays interpreted — silently, forever.

`ExecutableBuffer::wanted()` counts every byte codegen asked for, dropped
writes included, so it is not another guess. `lower_inner` is now a retry
wrapper: first attempt at the estimate, and on a code-buffer refusal one re-run
at the measured size + 1/8 + 256 bytes, capped at 4 MiB. Reserved bytes count
against the code-cache cap, so raising the constant to cover the worst method
would tax every ordinary one.

The first attempt's overflow is a measurement, not a failure, so its warnings
are suppressed (`ExecutableBuffer::set_quiet_overflow`); a retry that overflows
*again* still says so. `ir_code_buffer_retries()` counts the retries, because
without it "the retry never fired" and "the retry fired and worked" produce the
same log.

### 2b. The single-pass backend's invoke allowance

`invoke_info.len() * 512` → `* 1024`. Ten methods overflowed per run and in
every one of them `inline_extra` was 0, so the whole shortfall sat in that one
term. Solving each for the per-invoke cost the body actually needed gives
515–957 bytes (the arithmetic and the ten methods are in the comment at the
site). This backend cannot retry: it consumes six one-shot thread-local staging
requests before the buffer is allocated, so re-entering it would find them
gone. Its estimate has to be right the first time, and now errs high.

### Result

Same class, same classpath, one run each:

| binary | overflow warnings | x64 named bails | optimizing-tier bodies |
|---|---|---|---|
| dev `b56da0bba1` | 8082 | 10 | 938 |
| + ir-lower retry | 1080 | 10 | 1033 (97 retries, 95 succeeded) |
| + invoke allowance 512→1024 | **0** | **0** | **1109** |

171 more compiled bodies per run, and the warning channel is quiet again.

## 3. The `DeferredLogFactory` receiver mix-up — not reproducible

The original report also recorded, from a binary at `351218f44` (before
`7f1b1f263`):

```
NoSuchMethodError method="java/lang/Class.getLog(Ljava/util/function/Supplier;)Lorg/apache/commons/logging/Log;"
                  caller="org/springframework/boot/logging/DeferredLogFactory.getLog(Ljava/lang/Class;)... @pc=12"
```

`DeferredLogFactory.getLog(Class)` is a `default` interface method that
forwards to its own abstract overload `getLog(Supplier)`; the VM resolved the
call against the class of the ARGUMENT.

It does not reproduce on dev. Zero `NoSuchMethodError` of any kind in five full
runs of this class across four binaries (including the unfixed one) and a
HotSpot control. `probes/SelfOverloadReceiverProbe.java` drives the shape
directly — an interface `default` method forwarding to a same-named abstract
overload of itself, with a lambda capturing the `Class` argument, at a
polymorphic call site — for 400 000 iterations, and passes on HotSpot and on
both CratonVM binaries at the default threshold and at
`CRATONVM_JIT_THRESHOLD=5`.

Treat the signature as closed here. The nearest live relative is
`docs/known-issues/hibernate/gettypename-wrong-receiver-in-sessionfactory-rebuild-cascade-20260801.md`,
which is OPEN and has its own probes; it is a different resolution error (a
`Class` mirror resolved to the class it DESCRIBES, rather than an argument
taken as the receiver).

## Acceptance gate

`jit/src/lib.rs::direct_jit_callee_calls_enabled()` asks for
`BasicErrorControllerIntegrationTests`, default flags, 14 consecutive clean
runs, plus a same-binary gate-closed control. Run on the fixed binary:

| arm | attempts | clean |
|---|---|---|
| default flags | 17 | **14 x PASS 26/26** |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` (gate-closed control) | 2 | **2 x PASS 26/26** |

Fourteen clean runs, but **not fourteen consecutive ones**, and the difference
is worth stating plainly. Three of the seventeen attempts were lost, every one
of them while the shared 16-core host was carrying an external load average
above 100 (peaks of 266 from other tenants); all fourteen clean runs happened
below ~60, and three replacement runs at load ~35 took about five minutes each
and passed 26/26.

* two runs **stalled** — one killed at the harness's 2400 s cap, one caught by
  `--stack-dump-on-timeout 1500`, which put `main` in `Thread.join()` under
  `OnClassCondition$ThreadedOutcomesResolver`. Filed separately:
  `docs/known-issues/springboot/onclasscondition-join-never-returns-20260801.md`.
  It is not this defect and not the direct-call gate — it is a hang in Spring
  Boot's two-thread auto-configuration filtering.
* one run failed a single test on a **client-side**
  `HttpClient request timed out` (`testRequestBodyValidationForMachineClient`),
  the request never reaching a response at load 147.

What the gate was for is settled either way: the gate-open and gate-closed arms
are both clean, so nothing here reads against the reopened direct-call edge.
That was exactly the confusion this report was filed to prevent.

## Still open on this class, and untouched here

This report was always a narrow companion to two others, and it did not re-file
them. Neither is closed by this work, and neither was seen in any of the 20+
runs recorded above:

* [`../../known-issues/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731.md`](../../known-issues/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731.md)
  — the `checkcast: not an object reference` hard abort, a GC defect (a
  collection-overlay backing array reclaimed while still live). A different
  failure mode entirely: that one kills the process, this one failed
  assertions.
* [`../../known-issues/springboot/basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md`](../../known-issues/springboot/basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md)
  — the original 2026-07-28 report and its `ConditionEvaluationReport`
  residual.

## Reproduction

The Windows harness in the original report still applies. On the Linux host the
same class runs directly through the JUnit launcher:

```bash
D=/data/data/spring-boot-tomcat-crossmodule-20260717
CP="$(cat $D/module/spring-boot-webmvc/build/cratonvm-test-cp.txt):$D/sb-runner"
CLS=org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests
cd "$D" && <cratonvm> -Xmx2g -cp "$CP" SbRunner "$CLS"
```

A failing run takes ~30 s (every context boot dies immediately); a passing one
takes ~10-15 min.

## Files

* `vm/src/runtime/interpreter.rs` — `run_jit_callee_handler`
* `jit/src/lib.rs` — `handler_reads_non_param_local`, `ExecutableBuffer`
  (`tag`, `quiet_overflow`), `ir_code_buffer_retries`
* `jit/src/ir_lower.rs` — `lower_inner` / `lower_inner_sized`
* `jit/src/x64.rs` — the single-pass buffer estimate
* `probes/SelfOverloadReceiverProbe.java`
