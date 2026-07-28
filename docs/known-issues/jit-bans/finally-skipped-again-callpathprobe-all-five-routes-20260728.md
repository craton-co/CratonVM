# `finally` is skipped again on all five dispatch routes (CallPathProbe fully regressed)

**Status:** OPEN. Found 2026-07-28 while fixing the handler-liveness bug in the
same family. Reproduced on **clean `origin/dev`** (`075fdfc54`) with no local
changes — this is not a side effect of that fix.

## Symptom

`docs/known-issues/repros/jitban-remaining-20260726/CallPathProbe.java`, the
regression guard commit `66548471f` added for itself, now leaks on **every**
route:

```
STATIC-direct          final=28440    LEAK
IFACE-class-delegating final=56980    LEAK
IFACE-class-inline     final=28490    LEAK
LAMBDA-methodref       final=28571    LEAK
LAMBDA-body            final=28497    LEAK
```

`66548471f`'s own commit message records the state it left behind:

```
STATIC-direct           OK        (was LEAK)
IFACE-class-inline      OK        (was LEAK)
IFACE-class-delegating  OK        (was LEAK)
LAMBDA-methodref        LEAK      (open)
LAMBDA-body             LEAK      (open)
```

So the three routes that commit fixed have all regressed back. `FinallyBalanceProbe`
agrees — it is the witness that commit was built around, and it leaks again:

```
FLAT   final depth=57040 (expect 0) firstLeak=724
NESTED final depth=57066 (expect 0) firstLeak=542
```

versus the `depth=0` that commit reported.

## How this was established

Two binaries built from the **same** worktree at the same commit, differing only
in whether the (unrelated) handler-liveness change was applied — `git stash` /
rebuild / `git stash pop`. Both produce the numbers above, within run-to-run
noise. There is no configuration in which the probe passes here.

This matters because the obvious reading — "the new change broke the finally
routing" — is wrong, and a bisect that starts from that assumption will burn a
lot of build cycles.

## Why it is not the handler-liveness fix

That fix only ever *adds* liveness and interference edges
(`regalloc::handler_live_mask`); it does not touch exception routing,
`route_jit_signal_exception`, `run_jit_callee_handler`, or any dispatch path.
And the leaks are byte-for-byte present with it stashed.

## Where to look

`66548471f` fixed this with two mechanisms, either of which could have been
undone or bypassed since:

1. **Throw-pc stamping.** `emit_exception_check_stub` emits one pad per distinct
   throw-site bci that calls `JitRuntimeHelpers::set_throw_bci` before returning
   the `i64::MIN` sentinel, so `JitSignals::athrow_bci` names *this* method's
   throw site rather than whatever the callee's `athrow` lowering left there. A
   catch-all (`catch_type == 0`, i.e. a javac `finally`) has nothing but the pc
   range to match on, so a foreign bci silently drops it. Check that the stub is
   still reached on these routes and that nothing re-shares a single stub across
   bcis.
2. **Resume-at-handler.** `interpreter::run_jit_callee_handler` replaced
   whole-method re-execution. Note it is NOT firing for the shapes in
   `C2Handlers` (traced with `CRATONVM_DBG_RBC6=1`: only
   `route_jit_exception_through_method` appears), so confirm which of the two
   drains these probes actually take.

Useful flags: `CRATONVM_DBG_RBC6=1` (routing decisions, throw pc, handler pc),
`CRATONVM_DBG_EXCFRAME=1` (locals dropped from an exceptional-frame snapshot).

## Scale

A skipped `finally` is a silent resource/state leak in ordinary Java —
`66548471f` traced it to javac's own `ClassFinder.complete` leaving annotation
processing permanently blocked, which is the root cause behind
SPRING-TESTCOMPILER.3. That whole argument applies again.
