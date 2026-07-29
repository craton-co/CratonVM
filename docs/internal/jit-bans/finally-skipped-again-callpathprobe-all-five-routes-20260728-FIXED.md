# `finally` is skipped again on all five dispatch routes (CallPathProbe fully regressed)

# FIXED 2026-07-29

The lambda and method-reference JIT entry now resumes the target method at its
own exception handler using the stamped throw BCI, rather than falling through
to a fresh method-entry execution. The ClassFinder.complete JIT ban was
removed.

Validated on Azure with cratonvm-finally-019faeeb-r2
(SHA-256 933859499a13bb8e9b70bf5192ec3215f488459e38895a4eb080a7fbe3dd30d1):
CallPathProbe, FinallyBalanceProbe, FinallyShapeProbe, and
FinallyThrowSiteProbe all passed at 200000 iterations in JIT and no-JIT modes.

**Status:** FIXED and retired 2026-07-29. Found 2026-07-28 while fixing the handler-liveness bug in the
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

---

## Update 2026-07-28 — three of five routes fixed; the two lambda routes remain

Two independent defects, both the same shape: **`66548471f` widened the PRODUCER
of the throw bci but left two CONSUMERS asserting the old, narrower meaning.**
Fixing both restores every non-lambda route.

| | before | after |
|---|---|---|
| `FinallyBalanceProbe` FLAT / NESTED | 57038 / 57065 | **0 / 0** |
| `CallPathProbe` STATIC-direct | LEAK | **OK** |
| `CallPathProbe` IFACE-class-inline | LEAK | **OK** |
| `CallPathProbe` IFACE-class-delegating | LEAK | **OK** |
| `CallPathProbe` LAMBDA-methodref | LEAK | LEAK (route 3) |
| `CallPathProbe` LAMBDA-body | LEAK | LEAK (route 3) |

A/B on one worktree at one commit, only these edits differing.

### Defect 1 — `jit_local_athrow_pc` still demanded a literal `athrow`

```rust
if pc < code_len && cached.code[pc] == 0xbf { pc } else { usize::MAX }
```

That opcode test dated from when a local `athrow` was the only site that stamped
a bci. `66548471f` made `emit_exception_check_stub` emit one pad per distinct
throw-site bci calling `set_throw_bci`, across all 19
`emit_post_invoke_exception_check` sites plus `emit_post_alloc_oom_check` — so
the stamped bci is now usually an **invoke**. `FinallyBalanceProbe.guarded`
stamps bci 9 (`invokestatic work`); the test rejected it, returned `usize::MAX`,
and `find_jit_exception_handler` took its pc-unknown path — which deliberately
skips a catch-all whose region does not span the whole method, i.e. every javac
`finally`.

Replaced by a strictly better foreign-bci filter: the pc must fall **inside one
of this method's own protected ranges** (and be a real instruction boundary).

A boundary test alone is NOT enough, and the suite caught that: `athrow_bci`
carries no method identity, so a callee's stamp can still be standing when this
method drains. `JitPreciseHandlerFrame.plainStep` (range [0,4)) receives its
callee `maybeThrow`'s athrow bci 13 — a perfectly valid boundary in `plainStep`
too — and `test_compiled_callee_catches_its_own_athrow` exists for exactly that.
The range test rejects 13 against [0,4) and falls back to the pc-unknown
sentinel, where a TYPED handler still matches by class; it accepts `guarded`'s 9
against [0,12), so the `finally` runs.

Rejecting an out-of-range pc costs nothing even when the pc is genuine:
`find_jit_exception_handler` would find no covering entry for it anyway, and the
one thing the pc-unknown path still honours — a catch-all spanning the whole
method — covers every pc by definition.

**Root note for whoever touches this next:** this family has now recurred three
times from one cause — `athrow_bci` carries no method identity, so every
consumer re-derives "is this pc mine?" heuristically. The durable fix is to
stamp an identity alongside the bci (an ABI change to
`JitRuntimeHelpers::set_throw_bci`) rather than keep refining the heuristic.

### Defect 2 — `route_implicit_exc_through_callee` cleared the bci before using it

Its first statement is `clear_jit_athrow_bci()`, justified because the bci names
the *callee* and would be foreign to the caller. But the KCFULL-13 branch a few
lines below routes through **that same callee's own exception table**, for which
the bci is exactly right. Measured: 4213 entries to the branch, `athrow_bci=-1`
at every one, 0 handlers found, and the fall-through re-ran the callee from its
entry — leaking one increment per throw (the ~2x count on IFACE-class-delegating
is the compiled attempt's leak plus the re-run's balanced pass).

Fixed by capturing the bci before the clear (`peek_jit_athrow_bci`) and using it
only in that branch. The clear itself stays, so every propagate-outward and
re-run path behaves exactly as before.

This is also why `run_jit_callee_handler` — `66548471f`'s resume-at-handler fix —
had been firing **zero** times: it is reached only through that branch, and the
branch could never find a handler.

### Route 3 (LAMBDA-methodref, LAMBDA-body) — still open, now better localised

`FinallyThrowSiteProbe` and `FinallyShapeProbe`'s FINALLY row are also this route
(their drivers are `Probe::method` references), which is why they still leak:

```
FinallyShapeProbe     FINALLY LEAK; CATCH / CATCHALL / CATCHRETHROW / NOTHROW OK
FinallyThrowSiteProbe all three LEAK (all driven via `FinallyThrowSiteProbe::x`)
```

The passing rows are consistent: a *typed* handler still matches by exception
class under an unknown pc, and a catch-all spanning the whole method is honoured;
only the narrow catch-all needs the pc.

What is now established about route 3:
- the lambda target IS compiled (`full-compile FinallyThrowSiteProbe.direct(I)V
  entry=... len=1758`);
- it is invoked through `try_lambda_dispatch` ->
  `invoke_shared` / `invoke_on_class_shared`;
- **no drain consults its table at all** — `CRATONVM_DBG_RBC6=1` over 20k
  iterations shows 2759 leaks against 74 total routing events, and
  `FinallyThrowSiteProbe.direct` never appears in a single
  `route_jit_signal_exception` / `route_jit_exception_through_method` line.

So the exception escapes the compiled body without ever reaching
`execute_jit_call`'s drain. The next step is to find which JIT entry
`invoke_shared` takes for a lambda target and give it the same drain the
ordinary path has. Note DIRECT-athrow leaks too, so this is not about the bci at
all — that shape's pc was always known.

### Harness trap that caused a spurious revert of this fix

This fix was landed (`e3ef0409f`), reverted (`8ea368974`), then re-landed
(`bb966dbcc`). The revert was based on a bad measurement, not a real defect.

`jit_interp_differential` **spawns `target/release/cratonvm.exe`** as a child
process. A verification command shaped as

    cargo test -p cratonvm-vm --test jit_interp_differential && cargo build --bin cratonvm

therefore runs the suite against the PREVIOUS build's binary. Both the SIGSEGV
that triggered the revert and the follow-up "baseline passes with the change
reverted" run — which appeared to confirm it — were measuring stale binaries, in
opposite directions. Neither attribution was valid.

Re-measured with the binary built FIRST: `jit_interp_differential` 10/10, the
three exception suites 19/11/11, and the `JitDifferential` fixture run directly
10 times with 0 failures.

**Rule: any suite that spawns the cratonvm binary must have that binary built
before the suite runs.** `cargo build --bin cratonvm` first, then `cargo test`.
