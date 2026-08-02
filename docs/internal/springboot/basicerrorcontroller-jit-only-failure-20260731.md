# `BasicErrorControllerIntegrationTests` is usable as an acceptance gate again

**Status: FIXED 2026-08-01.** Filed 2026-07-31 as "this class is NOT a usable
acceptance gate right now"; it is one again, and the gate it blocked has been
run.

The class failed 23 of 26 tests under JIT with default flags and passed 26/26
under `--nojit`, which made the acceptance line in
`jit/src/lib.rs::direct_jit_callee_calls_enabled()` — "14 consecutive clean
runs, plus a same-binary gate-closed control" — unrunnable. The report carried
three items. Item 1 was the failure; it was fixed on `dev` by separate work,
concurrently with this branch (see the note below — that matters for reading
the history). Item 2 was fixed here. Item 3 does not reproduce.

## 1. The JIT-to-JIT handler resume dropped every non-parameter local

**This was the failure**, and it is fixed: `interpreter::run_jit_callee_handler`
rebuilt a compiled callee's exception-handler frame from the callee's incoming
arguments and nothing else, so every other local came back null.

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
`try`, and the `catch` does not return — it falls through to the loop back
edge, which reloads local 5 and calls `hasNext()` on it.

### Localisation

Every step is a measurement on one binary (dev `b56da0bba1`, release, Linux
x86-64, real JDK 25, Spring Boot 4.1.0-SNAPSHOT), narrowing by JIT admission:

| arm | result |
|---|---|
| HotSpot 25 | PASS 26/26 |
| CratonVM, JIT on, default flags | **FAIL 26 tests / 23 failed** |
| `CRATONVM_JIT_DENY=org/springframework/boot/context/properties/bind/` | PASS 26/26 |
| `CRATONVM_JIT_DENY=java/util/` | FAIL 23 — not the JDK collections |
| `CRATONVM_JIT_DENY=bind/BindConverter` | PASS 26/26 |
| `CRATONVM_JIT_DENY=BindConverter.convert` | PASS 26/26 |
| `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES=1` | PASS 26/26 |

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
(`precise_handler_frames_enabled`, `jit/src/lib.rs`) stopped refusing them:
they compile on the promise that every throwing site in a protected range
publishes a reason-9 exceptional frame carrying the real locals.

Two sinks reconstruct the handler frame.
`interpreter::route_jit_signal_exception` — the interpreter-boundary drain —
was updated with that relaxation and consumes the precise frame.
`interpreter::run_jit_callee_handler` — the JIT-to-JIT dispatch resume reached
from `jit::helpers::route_implicit_exc_through_callee` — was not. Its own doc
comment still carried the retired argument:

> Locals are the callee's incoming arguments … sound for exactly the same
> reason: a compiled method whose handler reads a local first assigned inside
> the try never passes the `local_handler_reads_unsafe_local` compile gate.

That sentence stopped being true when the gate stopped refusing. Which sink
runs depends only on who called the method — the interpreter, or compiled code
through the dispatch helper — so the same method was correct on one path and
silently wrong on the other.

The `throw_pc=18446744073709551615` above (`usize::MAX`) is the same gap seen
from the other side: with no precise frame there is no throw bci either, so the
handler was picked by exception class alone.

### Two independent diagnoses, one fix

**This was found twice, on the same day, by two people who did not know about
each other, from three different witnesses.** Recording that plainly, because
the history is confusing otherwise:

* this branch, from `BasicErrorControllerIntegrationTests` (this report);
* concurrent work on `dev`, from `LiquibaseAutoConfigurationTests` (27 of 43
  methods) and `DevToolsPooledDataSourceAutoConfigurationTests`.

The `dev` implementation landed first and is the one in the tree. It is also
the better of the two, for a reason worth knowing: it repairs **slot 0** from
the caller's `incoming_args`. The reason-9 snapshot records `this` as
`Undefined` whenever liveness says the bytecode has no further *read* of it,
which is the common case and exactly what the real `BindConverter.convert`
frame does — `getfield delegates` at bci 3 is its last use, so local 0 is
dropped from bci 4 on. Liveness is the right answer for a bytecode read and the
wrong answer for the receiver, which the VM still needs for a `synchronized`
method's monitor and for stack traces. This branch's version did not restore
it, and would have handed some handlers a null `this`. That branch's
implementation was therefore dropped in the merge in favour of `dev`'s; what
this branch kept is:

* the two unit tests below, which `dev`'s side did not have;
* `helpers::route_implicit_exc_through_callee` dropping any standing
  exceptional frame *before* materializing an implicit NPE/AIOOBE. That
  allocation can run a young collection, and a `ReconstructedFrame` is not a GC
  root, so a frame left standing across it names relocated objects. Without one
  the resume fails closed instead, which is the conservative answer for that
  branch.

Tests added here: `jit/src/lib.rs`
`handler_falling_through_to_a_loop_back_edge_reads_the_iterator_local` and
`handler_that_returns_does_not_read_a_non_param_local`, which pin
`handler_resume_requires_precise_locals` on the exact `BindConverter` bytecode
shape (a handler whose trailing `goto` reaches a loop back edge that reloads
the iterator local) and on the negative control (a handler that returns, where
the `iload_1` past its own `ireturn` must NOT be scanned into).

## 2. The code-buffer overflow flood — and its misattribution

The original report recorded a flood of

```
JIT try_patch_i32: offset out of bounds; marking buffer overflowed offset=4094 len=4094
```

and attributed it to the single-pass backend's
`ExecutableBuffer::new(estimated_size.max(4096))` in `x64.rs`.

**That attribution was wrong**, and it was wrong for a structural reason: the
warning named neither the buffer's capacity, nor the size the body needed, nor
which of the four sizing heuristics allocated it — so it could only be
attributed by arithmetic on `len`, and `len` freezes at the first dropped
write, which makes it the one number that cannot answer the question. The
warnings now carry `capacity`, `wanted` and a `buffer` tag. Measured on one run
of this class:

| source | warnings per run |
|---|---|
| `ir-lower` (the optimizing tier) | **8072** |
| `x64-single-pass` | 10 |

### 2a. The optimizing tier now measures instead of guessing

`ir_lower`'s buffer was sized `nodes * 32 + calls * 448 + 1024` — one number
for a call site whose real cost swings by several hundred bytes depending on
which lowering it picks, and widening the PIC's inter-slot branch from `rel8`
to `rel32` (part of `7f1b1f263`) pushed the expensive end past it.
`ExecutableBuffer::emit` then drops the write, sets the sticky `overflowed`
flag, and the method stays interpreted — silently, forever.

`ExecutableBuffer::wanted()` counts every byte codegen asked for, dropped
writes included, so it is not another guess. `lower_inner_with_scopes` is now a
retry wrapper: first attempt at the estimate, and on a code-buffer refusal one
re-run at the measured size + 1/8 + 256 bytes, capped at 4 MiB. Reserved bytes
count against the code-cache cap, so raising the constant to cover the worst
method would tax every ordinary one.

The first attempt's overflow is a measurement, not a failure, so it is silenced
(`ExecutableBuffer::set_quiet_overflow`); a retry that overflows *again* still
warns. `ir_code_buffer_retries()` counts the retries, because without it "the
retry never fired" and "the retry fired and worked" produce the same log.

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
[`../../known-issues/hibernate/gettypename-wrong-receiver-in-sessionfactory-rebuild-cascade-20260801.md`](../../known-issues/hibernate/gettypename-wrong-receiver-in-sessionfactory-rebuild-cascade-20260801.md),
which is OPEN and has its own probes; it is a different resolution error (a
`Class` mirror resolved to the class it DESCRIBES, rather than an argument
taken as the receiver).

## Acceptance gate

`jit/src/lib.rs::direct_jit_callee_calls_enabled()` asks for
`BasicErrorControllerIntegrationTests`, default flags, 14 consecutive clean
runs, plus a same-binary gate-closed control.

**MET, on the final tree** (`origin/dev` `5443fae920` + this branch), Linux
x86-64, real JDK 25, 3 concurrent, one binary:

| arm | runs | clean |
|---|---|---|
| default flags | 14 | **14 consecutive** |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` (gate-closed control) | 2 | **2** |

**Both arms clean is what settles the question this gate asks**, and it is the
answer this report was filed to make obtainable: the class's failure was never
about the direct-call edge, and anyone reading a red run of it as evidence
against that edge between 2026-07-31 and 2026-08-01 was reading the wrong
defect.

**The class is not immune to two intermittent failures**, and a 14-run gate
will not always come back clean. Earlier rounds on this branch lost one run to
a stall — `main` parked in `Thread.join()` inside Spring Boot's two-thread
`OnClassCondition` filtering, caught by `--stack-dump-on-timeout 1500` — filed
as
[`../../known-issues/springboot/onclasscondition-join-never-returns-20260801.md`](../../known-issues/springboot/onclasscondition-join-never-returns-20260801.md).
Another lost one run to a SIGSEGV in an unmapped code buffer, filed as
[`../../known-issues/jit/sigsegv-in-unmapped-code-buffer-20260801.md`](../../known-issues/jit/sigsegv-in-unmapped-code-buffer-20260801.md),
and one to a client-side `HttpClient request timed out` at external load 147.

Neither intermittent failure is attributable to this work on the evidence
collected. Across 2026-08-01, on the same host and fixture: **54 runs of this
branch produced 2 events; 20 runs with the branch's only JIT-churn change
switched off produced 0; 34 runs of pristine `origin/dev` produced 0.** A
same-binary A/B of that change — 20 interleaved on/off pairs — came back 20/20
clean on both arms, with the lever verified live first (96 retries to 0, 1109
to 1044 optimizing-tier bodies). Those numbers are consistent with a single
underlying rate and cannot separate the trees; they are recorded here so the
next person starts from data rather than from this report's silence.

## Still open on this class, and untouched here

This report was always a narrow companion to two others, and it did not re-file
them:

* `springboot-basicerrorcontroller-checkcast-abort-20260731` — the
  `checkcast: not an object reference` hard abort, a GC defect (a
  collection-overlay backing array reclaimed while still live). **Fixed
  2026-08-01 by separate work** and retired to
  `docs/internal/fixed-suite-bugs/springboot/`.
* [`../../known-issues/springboot/basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md`](../../known-issues/springboot/basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md)
  — the original 2026-07-28 report and its `ConditionEvaluationReport`
  residual. Still OPEN.

## Reproduction

The Windows harness in the original report still applies. On the Linux host the
same class runs directly through the JUnit launcher:

```bash
D=/data/data/spring-boot-tomcat-crossmodule-20260717
CP="$(cat $D/module/spring-boot-webmvc/build/cratonvm-test-cp.txt):$D/sb-runner"
CLS=org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests
cd "$D" && <cratonvm> --stack-dump-on-timeout 1500 -Xmx2g -cp "$CP" SbRunner "$CLS"
```

A failing run took ~30 s (every context boot died immediately); a passing one
takes ~5 min on an idle host. Always arm `--stack-dump-on-timeout` — see the
stall report cited in the gate section.

## Files

* `jit/src/lib.rs` — `ExecutableBuffer` (`tag`, `quiet_overflow`, the
  capacity/wanted fields on the warning), `ir_code_buffer_retries`, the two
  predicate tests
* `jit/src/ir_lower.rs` — `lower_inner_with_scopes` / `lower_inner_sized`
* `jit/src/x64.rs` — the single-pass buffer estimate
* `vm/src/jit/helpers.rs` — `route_implicit_exc_through_callee`
* `probes/SelfOverloadReceiverProbe.java`
