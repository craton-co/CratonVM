# RBC.6's field-op admission let an NPE escape its handler — the frame was published, then deliberately discarded

| | |
|---|---|
| **Status** | **FIXED 2026-08-18** |
| **Opened** | 2026-08-18, as `known-issues/jit/rbc6-getfield-putfield-admission-lets-an-npe-escape-its-handler-20260818.md`, found running `Rbc6FieldProbe` as a regression control for the `new`/`athrow` admission |
| **Fixed by** | `fix/rbc6-getfield-npe-escape-20260818` |
| **Acceptance** | `Rbc6FieldProbe` and `Rbc6FieldBisect` match HotSpot **exactly** under JIT — `escapes=0` on all five rungs, where it was 24 539 of 25 000 — while `c2=6` and `hot_but_stuck_in_interpreter=0` prove the methods still compile |

## What was wrong with the original diagnosis

The page that opened this said the 2026-08-02 admission argument — "`getfield`
and `putfield` already publish a precise reason-9 frame on every throwing path"
— "does not hold", and told the next reader to find which of the four `getfield`
sub-paths a compiled entry selects and whether `emit_post_invoke_exception_check`
really follows it there.

**That argument holds, and every sub-path does follow it.** The instruction was
pointed at the wrong half of the pipeline. Two traces settle it before any code
is read:

```text
# CRATONVM_DBG_RBC6_EMIT — what the emitter chose
[rbc6-emit] post_invoke_exc_check method=…getfieldIntHandlerLocal:(…)I bci=10
            ret=I precise_req=true protected=true -> REASON9

# CRATONVM_DBG_DEOPT — what the stub did at run time, 4 608 times
[cratonvm-deopt] x64 exceptional frame at throw bci=10
                 locals=[Undefined, Undefined, Int(3587), Undefined]
```

`Int(3587)` is `scratch` for `n = 512` (`n*7+3`), at the right bci, in the right
slot. The frame is emitted, built, and stashed **correctly**. The defect is
entirely downstream.

## The actual defect

`route_implicit_exc_through_callee` (`vm/src/jit/helpers.rs`) threw the frame
away, one statement before it was needed:

```rust
// Drop any exceptional frame the abandoned compiled attempt published
// BEFORE materializing the exception. `create_exception_object` allocates
// on the Java heap and can therefore run a young collection, and a
// `ReconstructedFrame` is not a GC root — its object words would survive
// as stale addresses.
cratonvm_jit::deopt::clear_exceptional_frame();
```

With the frame gone, `run_jit_callee_handler` does exactly what it should:

```text
[rbc6-dbg] run_jit_callee_handler DECLINED …getfieldIntHandlerLocal(…)I
           throw_pc=-1 — handler needs precise locals and no frame was published
```

and the NullPointerException propagates past a `catch (NullPointerException)`
that catches it. **The runtime was never the bug**; its refusal is the
fail-closed guard working as designed on an input that should never have
occurred.

**The defence had outlived its cause.** A `ReconstructedFrame` in
`LAST_EXCEPTIONAL` *is* a GC root: the scan half runs in `memory/roots.rs` §10
(`for_each_stashed_deopt_object`), the remap half in `memory/gc.rs`
(`remap_stashed_deopt_objects`), and a debug assertion refuses one without the
other. Both land on the thread that owns the stash — which is the thread doing
the allocation. `docs/jit/deopt-thread-local-roots.md` names **this exact
window** as the shortest instance of the hazard that wiring closed: a reason-9
frame published by `emit_post_invoke_exception_check` whose sink allocates the
throwable before draining it.

The fix is one deleted statement. The other three `clear_exceptional_frame()`
calls in the same function stay: each runs where the compiled attempt is
FINISHED or ABANDONED, so its frame can never be legitimately claimed.

## Two things the opening page got wrong, and one word

* **It is not getfield-specific.** `Rbc6FieldBisect.java` attributes per rung:
  all five escape — `getfield` int/ref/long, `putfield`, and the two-locals
  variant — because they share one route, not one emitter. Any implicit
  AIOOBE is in the same population.
* **It is not intermittent.** Escapes begin at the iteration the methods
  compile (3 552 / 3 688) and then occur on **every** null-receiver iteration:
  24 539 of 25 000. "10 iterations pass, 200 000 fail" is a compilation
  threshold, not a race.
* **"Conservative" was the wrong word** in the deleted comment. Refusing to
  enter a handler is not a safe subset of entering it — JVMS requires it to run
  — so the refusal is a wrong answer that happens to be loud (an escaping
  exception) rather than quiet (zeroed locals). Calling it conservative is what
  made it look like a defensible trade.

## Why the stale defence looked justified

`docs/jit/deopt-thread-local-roots.md` opened with **"The `vm/` half is not
wired"**. All four call sites it listed as outstanding had since landed; only
the status line had not been updated. A reader checking whether the frame was
safe to keep found an authoritative "no".

That line is corrected, and `vm/tests/deopt_stash_roots_wired.rs` now pins the
wiring the fix depends on: unwire either half and the test names which one and
why, instead of the next miscompile finding out.

## Verification

| arm | `acc` | escapes |
|---|---|---:|
| HotSpot 25 | `3505302427599075008` | 0 |
| CratonVM JIT, **fixed** | `3505302427599075008` | **0** |
| CratonVM `--nojit` | `3505302427599075008` | 0 |
| CratonVM JIT, before | *(uncaught NPE)* | 24 539 |

Non-vacuous by construction — a refused method is an interpreted method and an
interpreted method is already correct, so the green means nothing without proof
the bodies compiled:

* `jit-method-stats`: `c2=6`, `still-interpreted=1` (`main`),
  `hot_but_stuck_in_interpreter=0`.
* `CRATONVM_DBG_RBC6`: `run_jit_callee_handler … precise=true` **3 357 times**,
  and zero `DECLINED`. Before the fix every one of them was `DECLINED`.
* `CRATONVM_DBG_RBC6_EMIT`: the emitter still chooses `REASON9`, so the fix did
  not quietly move the sites onto the shared sentinel stub.

## The blast radius was nine test suites, not one probe

The probe found it; it was never confined to the probe. Running the same scoped
set (`cratonvm-vm`, `-jit`, `-gc`, `-types`, `-cli`) at this branch's merge base
`c3c593f01` and on the fix: **zero regressions, and nine suites that fail at the
base pass with it.** Each was re-run individually on both trees to rule out
flakiness — all nine are deterministic:

| suite | at `c3c593f01` | with the fix |
|---|---|---|
| `clinit_first_call_compile_order` | FAILED | ok |
| `jit_cold_new_cp` | FAILED (2 of 5) | ok (5/5) |
| `jit_collection_ctor_identity` | FAILED | ok |
| `intrinsic_diff` | FAILED | ok |
| `conscrypt_logger_dispatch` | FAILED | ok |
| `fjp_recursive` | FAILED | ok |
| `threadpoolexecutor_prestart_regression` | FAILED | ok |
| `vthread_probe_regression` | FAILED (1 of 3) | ok (3/3) |
| `wave1_c_executor` | FAILED (4 of 5) | ok (5/5) |

That is what "every compiled `try`-wrapped field access in the tree" means
concretely: executor shutdown paths, a ForkJoinPool recursion, a virtual-thread
probe, a `<clinit>` ordering test and a logger dispatch all depend on a handler
that was being skipped.

**A tenth suite is NOT claimed.** `cratonvm-jit --lib` appeared in the same
delta but passes 1994/1994 on BOTH trees when re-run — it was flaky in the bulk
run, not fixed here. It is called out because a 10-row table would have been
easier to write and one row of it would have been false.

## Repro

```bash
javac -d /tmp/p probes/Rbc6FieldProbe.java probes/Rbc6FieldBisect.java
JDK=<jdk25>
<cv-bin> --java-home "$JDK" --Xmx 1500m -cp /tmp/p Rbc6FieldBisect 200000   # escapes must be 0
CRATONVM_DBG_RBC6_EMIT=1 <cv-bin> ... Rbc6FieldBisect 3000   # emitter: REASON9
CRATONVM_DBG_DEOPT=1     <cv-bin> ... Rbc6FieldBisect 3000   # stub: frames stashed
CRATONVM_DBG_RBC6=1      <cv-bin> ... Rbc6FieldBisect 6000   # sink: precise=true
```

`CRATONVM_DBG_RBC6_EMIT` is new here. It exists because the runtime `[rbc6-dbg]`
family cannot distinguish "the site chose the shared sentinel stub" from "the
site chose the reason-9 stub and the frame was lost afterwards" — and this bug
was entirely the second. Reach for it before reading any lowering.
