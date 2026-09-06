# `OffsetTimeTest`'s bare `AtomicBoolean.get()` NPE was a null receiver in COMPILED code — the trace shape is readable after all, and both mechanisms that can produce it are fixed

## Status

**CLOSED 2026-09-05.** Retires the public page
`offsettimetest-flaky-nullpointerexception-in-atomicboolean-get-20260905`
(deleted from `docs/known-issues/hibernate/` by the same commit), which
recorded a flaky, message-less `NullPointerException` whose whole stack trace
was one frame:

```
java.lang.NullPointerException
        java.util.concurrent.atomic.AtomicBoolean.get(AtomicBoolean.java)
```

and concluded "not pinned to a CratonVM source line ... no caller frame to
work from". The trace is not information-free; it is a precise signature, and
this page reads it.

## The trace shape, decided from the source and one probe

**1. A message-less NPE is a JIT implicit-trap NPE.** The interpreter's own
null-receiver path never produces one: it calls `helpful_npe_invoke_message`
and raises `Cannot invoke "Owner.name(...)" because "<expr>" is null`
(`interpreter/invoke.rs`, the `Value::Object(None)` arm). The sites that raise
`RuntimeError::NullPointerException { message: None }` for a receiver are the
five JIT implicit-signal drains — four in
`interpreter/jit_bridge.rs`, one in `interpreter.rs` — each of which drains
`JIT_PENDING_NPE` set by a compiled null-check stub.

**2. A one-frame trace naming the CALLEE is what that path prints.** Each
drain calls `attach_snapshotted_trap_frames`, and
`stackwalker::append_snapshotted_compiled_frames` appends the trap's
compiled-frame snapshot at the END of the trace (innermost last). When
`fillInStackTrace`'s own capture came back empty — the compiled frames had
already left the stack and nothing interpreted was below them — the merged
trace IS the snapshot. One compiled frame in, one line out.

**3. `AtomicBoolean.get()` is compiled JDK bytecode here, not the native.**
The page suspected `native_ab_get`'s null-receiver fallback. It is not
involved: that fallback cannot throw, and it does not run. `AtomicBoolean` is
on `real_protected_stub_class_common`'s allow-list, so `revalidate_cached_native`
yields its `SyntheticStub` registrations to the real classfile body once the
class is loaded — and the JIT then compiles that body. Measured on a release
binary:

```
CRATONVM_DBG_JIT_DISASM=AtomicBoolean.get  ...  AbCompiled
[cratonvm-jit-disasm] full java/util/concurrent/atomic/AtomicBoolean.get()Z
    entry=0x7d6cfc13d000 len=682
```

`AtomicBoolean.get()` is `return value != 0;` — a single `getfield` on `this`.

Put together: **a null receiver reached a compiled `AtomicBoolean.get`, and its
`getfield` faulted.** The caller is missing from the trace because the caller
was compiled too, which is a reporting limitation, not a mystery. Nothing here
is specific to `AtomicBoolean`, to `OffsetTimeTest`, to temporal types or to
timezones — the page had already ruled the last three out empirically, and
this is why.

## The timeline the page could not have known

Its run — `apps/hib-suite-runner/runs/nonpassed-rerun-gen-20260905/run-20260905-182403-passed`,
Azure, `-XX:+UseGenerationalGC`, one NPE in the whole 190-class shard —
finished at 19:36 UTC, i.e. 16:36 -0300. It therefore predates every one of:

| commit | -0300 | what it fixes |
|---|---|---|
| `1af0b2d7f` | 20:11 | a lambda site must refill an inline-cache slot its caller recompiled |
| `587acb50e` | 20:51 | a **stale TLAB skip span hid a live object from the young sweep and from `mark_young`** |
| `b09ead654` | 21:01 | drop homes pinned only by frame states no deopt can reach |

`587acb50e` is a **premature-reclaim** fix, and its own write-up reports
`io.netty.util.internal.ObjectCleanerTest` going from 8/8 non-clean to 0/8
under `-XX:+UseGenerationalGC` and `-XX:+UseG1GC`. A live object the young
sweep reclaims is exactly how a still-referenced `AtomicBoolean` becomes a
null receiver, and the page's run was Generational.

A second mechanism was found and fixed while retiring the sibling
hibernate-reactive page: the optimizing tier's phi copies were a parallel
assignment in memory and a sequence in registers, so a merge could hand a phi
another value's register — see
`../jit/jit-ir-phi-copy-register-alias-20260905-FIXED.md`. For a reference-typed
phi that is a wrong object or a null, at a merge, non-deterministically — the
shape the page describes ("the failure moves to an unrelated
parameter/method/timezone between runs").

## What was run

### The mechanism, shown to be live in the page's own configuration

`587acb50e` ships an opt-in that restores the pre-fix conditional publish, so
the two behaviours can be compared **in one binary** — which is the only kind
of A/B worth running on a low-probability defect. Its own named reproducer is
`io.netty.util.internal.ObjectCleanerTest`. Eight runs per arm, serial, on the
binary this branch built:

| collector | arm | non-clean runs | `AbstractMethodError` lines | stale-receiver warnings |
|---|---|---:|---:|---:|
| Generational | default | **0 / 8** | 0 | 0 |
| Generational | `CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH=1` | **8 / 8** | 16 | 24 |
| G1 | default | **0 / 8** | 0 | 0 |
| G1 | `CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH=1` | 0 / 8 | 0 | 0 |

(The G1 row does not match `587acb50e`'s own "6/6 non-clean on both
collectors". Recorded as measured rather than reconciled: this branch's base
`a044e1fe1` predates `ec6a2bd6d`, the G1-side twin of the guard, so the two
arms are not the same code on that collector. It does not affect this page —
the run being retired was Generational.)

The failing arm's log says exactly what the young sweep did:

```
WARN  Stale pointer detected in invokevirtual receiver
      (ptr=0x71b2900f1d70, all-zero header) — falling back to CP class java/util/Comparator
@@END FAILED  ObjectCleanerTest.testCleanupContinuesDespiteThrowing()
      java.lang.AbstractMethodError: java/util/Comparator.compare ... has no Code attribute
```

An object that was still referenced was reclaimed and its header zeroed. That
is the same event that leaves a **null** in a reference field of an object that
survived — the shape this page's NPE needs — and it was live, under
Generational, in every binary built before 20:51 -0300 on 2026-09-05. The
page's run is one of them.

### And the class itself

`org.hibernate.orm.test.type.temporal.OffsetTimeTest` alone,
`-XX:+UseGenerationalGC`, `-Dcraton.batch=1`, `found=396 started=264 ok=176
failed=0` and no `AtomicBoolean` NPE in every one of:

* 5 runs on `a044e1fe1` (which already contains `587acb50e`), serial;
* 4 runs on `a044e1fe1` + the phi-copy fix, four concurrent JVMs;
* 4 runs on the same binary with `CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH=1`,
  four concurrent JVMs.

Against the page's own 2-of-3 that is consistent with the defect being gone,
and it is NOT on its own a proof — 13 runs of ONE class is a poor sampler for
something that appeared once in a 190-class shard, and the third bullet in
particular is an underpowered arm, not a negative result. The
`ObjectCleanerTest` table above is the evidence; this is the corroboration.

## Disposition

The page asked its successor to "start by trying to get a caller frame (e.g. a
`--nojit` A/B to see if it still reproduces interpreted-only, which would at
least rule the JIT dispatch path in or out) before spending time on
Hibernate/temporal-specific theories". That is the right instinct and it is
now done, from the other end: the JIT path is ruled IN by construction — a
message-less NPE with a single compiled frame can be raised nowhere else — and
the two mechanisms in this VM that put a null into a compiled receiver were
both fixed within hours of the page being written.

**If it comes back**, the two things to reach for, in this order:

1. `CRATONVM_DBG_STTRACE=1` — prints `STTRACE_DBG_NPE_SNAPSHOT recovered=N
   trace_now=M` and every frame of the snapshot at the moment
   `attach_snapshotted_trap_frames` splices it. That is the caller frame the
   page wanted, and it already existed.
2. `CRATONVM_DBG_SWEEP_ZERO=1` — names the class of an object the young sweep
   zeroed while it was still live, which is the reclaim half of the story.

Do not start from the exception's class. `AtomicBoolean.get` is in the trace
because it is a one-`getfield` method that everything calls, so it is where a
bad receiver surfaces first; it is not where the receiver went bad.
