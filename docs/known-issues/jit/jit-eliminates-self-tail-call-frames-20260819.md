# The JIT eliminates self-tail-call frames, so a 4M-deep recursion returns where HotSpot overflows

**Status: OPEN — fully characterized, not yet fixed.** Mechanism, shape gate,
tier, promotion route and cost are all established and measured;
`CRATONVM_JIT_SELF_TAILCALL=0` now turns the lowering off, and it defaults ON.
Azure Linux host, branch `fix/jit-stackwalk-cross-chain-frames-20260819` off
`dev` `974471245`, one release binary for every arm below.

Split out of the stack-walking page now retired to
`fixed-suite-bugs/jit/jit-self-recursive-activations-invisible-to-stack-walks-FIXED-20260819.md`.
That page was about stack *walking* and its three mechanisms are fixed; this one
is about frames that are never pushed, which no walk can recover. It is also the
answer to that page's open question, "which emitter is responsible for the
pre-fix collapse".

## The measurement

`probes/SWTce.java` warms the shape up so it compiles, then drives it four
million deep:

```java
private static List<String> recurse(int d) {
    if (d == 0) return new ArrayList<String>();
    return recurse(d - 1);          // direct self TAIL call
}
```

```
depth=4000000  ->  CratonVM (JIT on)                       RETURNED n=0
                   CratonVM CRATONVM_JIT_SELF_TAILCALL=0   STACK_OVERFLOW
                   CratonVM --nojit                        STACK_OVERFLOW
                   HotSpot 25                              STACK_OVERFLOW
```

The compiled body reuses one frame per activation. The interpreter does not, and
neither does HotSpot.

## The mechanism

`jit/src/x64/bytecode_walk.rs`, the self-recursive `invokestatic` arm. When the
site qualifies, the compiler loads the argument slots into the parameter locals
and emits a `JMP rel32` back to `body_entry_offset` — the offset just past the
prologue, recorded at `jit/src/x64/driver.rs:1805`. No `CALL`, no new frame; the
next activation runs in the current one. The following `xreturn` is consumed
without being emitted.

It is deliberate. The arm is labelled "Tail-call optimization" and carries three
written refusals. Five conditions must hold, and each of the four that can be
varied from Java was measured below:

| # | condition | where |
|---|---|---|
| 1 | `invokestatic` whose class + name + descriptor equal the compiling method's | `is_recursive_call`, at all three compile doors |
| 2 | the byte at `pc+3` is `ireturn..areturn` (`0xac..=0xb0`) | `jit::invokestatic_self_call_uses_tail_jump` |
| 3 | that `xreturn` is not a branch target | `!branch_targets[pc + 3]` |
| 4 | the invoke pc is not covered by an exception handler | `!self.pc_is_protected(pc)` |
| 5 | the method has a prologue to jump back into | `body_entry_offset > 0` |

All three doors that can request a compile agree to leave such a site raw so the
backend reaches this arm, each spelling it `use_raw_tail_self_call`:
`jit::try_compile` (`jit/src/lib.rs`), the OSR path
(`vm/src/runtime/interpreter/jit_bridge.rs`), and the eager first-call compile
(`vm/src/runtime/interpreter.rs`).

## The kill switch

The defect had none, which is why the first round of measurement had to borrow
`CRATONVM_TIER_C2_THRESHOLD` as a stand-in lever. `CRATONVM_JIT_SELF_TAILCALL=0`
(token `-self-tailcall`, default ON) now demotes the tail-`JMP` to the ordinary
self-recursive `CALL`, so the two lowerings are A/B-able in one binary at one
tier. Do not confuse it with `CRATONVM_JIT_SP_TAILCALL`, which governs the
SIBLING tail-call (a `JMP` into *another* method's entry), or with
`CRATONVM_JIT_IR_SELFREC_DIRECT`, which governs the IR lowering — the arm that
*keeps* the frames.

Declared in `types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
`docs/flag-tokens.md` and `docs/config/flag-inventory.md`; the gate itself is
`x64::licm::self_tailcall_enabled`.

## Which tier: the fast one, and only the fast one

The lowering belongs to the single-pass/C1 backend. The optimizing IR tier
lowers the same self-call as a real `CALL`, so a method that gets an optimizing
body has its frames back.

`probes/SWTce3.java` drives five shapes 4,000,000 deep. They differ in exactly
one respect at a time:

| shape | descriptor | byte after the self-call | C1 pinned | default | `--nojit` | HotSpot 25 |
|---|---|---|---|---|---|---|
| `tailRef` | `(I)Ljava/lang/Object;` | `areturn` 0xb0 | **RETURNED** | STACK_OVERFLOW | STACK_OVERFLOW | STACK_OVERFLOW |
| `tailLong` | `(I)J` | `lreturn` 0xad | **RETURNED** | STACK_OVERFLOW | STACK_OVERFLOW | STACK_OVERFLOW |
| `tailVoid` | `(I)V` | `return` 0xb1 | STACK_OVERFLOW | STACK_OVERFLOW | STACK_OVERFLOW | STACK_OVERFLOW |
| `tailRefTry` | `(I)Ljava/lang/Object;` | `areturn`, inside a `try` | STACK_OVERFLOW | STACK_OVERFLOW | STACK_OVERFLOW | STACK_OVERFLOW |
| `tailInt` | `(I)I` | `ireturn` 0xac | STACK_OVERFLOW | STACK_OVERFLOW | STACK_OVERFLOW | STACK_OVERFLOW |

"C1 pinned" is `CRATONVM_TIER_C2_THRESHOLD=100000000`. Reading it:

* rows 1–2 against row 3 — the `0xac..=0xb0` window is real. `return` (0xb1) is
  one byte past it, so a `void` self-tail-call keeps every frame.
* row 1 against row 4 — the try-region refusal is real; the same body inside a
  `try` is not eliminated.
* row 5 — `(I)I` never sees the C1 lowering at all.
  `jit_bridge::promote_scalar_selfrec_to_ir` sends exactly the
  `static int f(int)` self-recursion shape to the optimizing pipeline on its
  first compile, threshold or not, and the optimizing pipeline emits a real
  `CALL`. It is the one row the tier lever cannot move.

`--nojit` and HotSpot 25 agree with each other on all five.

## How C2 actually arrives, and why some methods never get there

Only ONE tier enqueue is ever logged for a self-recursive method
(`[cratonvm-tier] enqueue … tier=C1 invocations=500`), yet an optimizing body
appears anyway. The C2 task comes from a second door that does not log:

1. `TieredCompilationManager::should_compile` enqueues the C1 task at
   `c1_threshold`. This is the only enqueue that prints.
2. On a successful non-optimized publish, `jit_bridge` computes
   `c2_upgrade_candidate = published && !optimized && c2_supersede() &&
   c2_upgrade_would_engage(...)`.
3. The worker loop then calls `CompilerCore::request_c2_upgrade`, which enqueues
   a **Low-priority C2 recompile directly on the core**, bypassing
   `should_compile` — so it never prints, and by default consults no invocation
   count whatsoever. Its own comment says so: "this one asked for nothing, which
   is what made that knob inert for the path that produces nearly all C2
   compiles."
4. `c2_upgrade_gate` arms a hotness gate on that supersede **only when
   `CRATONVM_TIER_C2_THRESHOLD` is set at all**. That is the whole reason the
   tier lever worked as a stand-in kill switch, and why `=1` is
   indistinguishable from the default while `=100000000` suppresses the
   supersede outright.
5. `jit::c2_upgrade_would_engage` returns false when the scan finds any
   `new_ops`, `anewarray_ops` or `indy_ops`. **A method that allocates is never
   a supersede candidate**, so it stays on the C1 body — and on the C1
   lowering — for the life of the process.

`SWTce`'s `recurse` allocates (`new ArrayList`) and is exactly that case.
`probes/SWTce4.java` sweeps a pause between the warm-up and the deep drive to
rule out a race with process lifetime:

```
pauseMs=0     depth=4000000 -> RETURNED n=0     (only bg-compile: tier=C1)
pauseMs=200   depth=4000000 -> RETURNED n=0     (only bg-compile: tier=C1)
pauseMs=1000  depth=4000000 -> RETURNED n=0     (only bg-compile: tier=C1)
pauseMs=3000  depth=4000000 -> RETURNED n=0     (only bg-compile: tier=C1)
```

No C2 task is ever created, at any pause, at any threshold. The permanent case
is permanent by admission, not by timing.

## Why it is intermittent, and when it is not

The window opens when the C1 body is published and closes when the supersede's
optimizing body replaces it. `probes/SWFrames.java` walks a 64-deep
self-tail-recursion 32 times and prints the frame count; HotSpot holds 68 on
every walk. One binary, 3 runs per arm:

| arm | walk#8 | walk#31 |
|---|---|---|
| default | flaky — `n=4` in 1 of 3 here, 7 of 10 on the pre-switch binary | `n=68` 3 of 3 |
| `CRATONVM_C2_SUPERSEDE=0` | **`n=4` 3 of 3** | **`n=4` 3 of 3** |
| `CRATONVM_JIT_SELF_TAILCALL=0` | `n=68` 3 of 3 | `n=68` 3 of 3 |
| both of the above | `n=68` 3 of 3 | `n=68` 3 of 3 |

Row 2 is the repro to use: suppressing the supersede pins the C1 body and the
flake becomes deterministic at both walks. Row 4 is the load-bearing one — same
tier as row 2, only the lowering differs, and the frames come back. That is the
defect isolated to one instruction sequence.

`n=4` is not a walk losing frames; the frames are not there. The chain dump
(`CRATONVM_DBG_SWCHAIN=1`) shows one chain entry and two activations found, the
outward RBP walk stopping because the next frame out really is the interpreter,
and the arithmetic agrees: 65 `recurse` frames at the 0xC0 bytes each measures
need 12480 bytes, and `entry_sp - scanner_sp` = 0x2BEF = 11247 bytes of stack
exist between the boundary and the scanner.

## Why `SWShape` did not reproduce — resolved

The first revision recorded that `SWShape` reports 67 for both its tail and
non-tail arms, matching HotSpot, and could not explain the difference from
`SWFrames`. It is condition 2. `SWShape.tail` is `(I)V`:

```
17: invokestatic  #38   // Method tail:(I)V
20: return               <- 0xb1, one past the window
```

against `SWFrames.recurse`, which is `(I)Ljava/util/List;`:

```
11: invokestatic  #9    // Method recurse:(I)Ljava/util/List;
14: areturn             <- 0xb0, inside the window
```

`SWShape` holds `tail=67 nonTail=67 chain=10` in 3 of 3 runs even with C1
pinned, so this is the shape gate and not tier timing. Non-tail self-recursion
cannot qualify by construction — the byte after the call is not an `xreturn`.

## What the lowering is worth

Both benches A/B `CRATONVM_JIT_SELF_TAILCALL` inside one binary, interleaved
arm-by-arm, and print the engagement readout (`ELIMINATED`/`FRAMES`, from a
2M-deep drive of the same compiled body) beside the number. Each carries a
non-tail control whose arm-to-arm delta is the noise floor. `sink` was identical
across every run of both benches — the answers do not change.

**`probes/SWTceBench.java`** — `static int tail(int, int)`, a supersede-eligible
shape, 4 interleaved rounds, ns per recursion level:

| configuration | ON | OFF | control (non-tail), ON / OFF |
|---|---|---|---|
| C1 pinned (`CRATONVM_TIER_C2_THRESHOLD=100000000`) | **0.71–0.79**, `ELIMINATED` | **4.06–4.45**, `FRAMES` | 3.53–3.94 / 3.56–3.96 |
| default | 5.40–6.06, `FRAMES` | 5.40–5.53, `FRAMES` | 3.54–3.58 / 3.52–4.12 |

At the C1 tier the lowering is worth **~5.8x** on the recursion itself. **At
default settings it is worth nothing at all** — both arms report `FRAMES` and
the same 5.4 ns/level, because the supersede has already replaced the C1 body
before the timed loop begins.

**`probes/SWTceBench2.java`** — `static Object tailAlloc(int, int)`, which
allocates at the base case and is therefore never a supersede candidate. No tier
lever set; this is the production configuration for the permanent population,
3 interleaved rounds:

| | ON | OFF |
|---|---|---|
| `tailAlloc` ns/level | **1.077–1.091**, `ELIMINATED` | **5.887–6.097**, `FRAMES` |
| `nonTailAlloc` control | 40.5–42.4 | 40.4–41.7 |

**~5.5x**, in production settings, with no flag set.

So the two populations divide cleanly, and unhelpfully:

* a method the supersede claims gets **no benefit** from the lowering (the C2
  body supersedes it) but still suffers the divergence during the C1 window —
  which is where the caller-sensitive APIs get their wrong answers;
* a method the supersede refuses gets the **full 5.5x** and suffers the
  divergence permanently, including a `StackOverflowError` that can never fire.

There is no configuration that buys the speed without the permanent loss. That
is the trade the fix has to price.

One side observation from the same runs: C1 *without* the elimination
(4.06–4.45 ns/level) beats the optimizing body (5.40–6.06) on this shape by
~1.25x. Same probe, same binary, only the tier pin differs. That is a separate
perf residual, not part of this defect.

## Why this matters beyond frame counts

`--nojit` and HotSpot agree with each other and disagree with the JIT, which
makes this a semantic divergence rather than a diagnostic one:

* **`StackOverflowError` never fires.** A program relying on it to bound a
  runaway recursion runs to completion under the JIT and dies under the
  interpreter, or vice versa. A genuinely unbounded self-tail-recursion becomes
  an infinite loop instead of a caught error. This is the permanent case above.
* **Every caller-sensitive API sees a different stack** — `StackWalker`,
  `Throwable.getStackTrace()`, `Reflection.getCallerClass`, security checks,
  Spring's `ControlFlowPointcut`.
* **The answers stay correct.** `SWValue` reports `wrongRounds=0/60` under JIT,
  `--nojit` and HotSpot alike, and both benches above return identical `sink`
  values in both arms. This is a frame-accounting divergence, not a computation
  one.

## The interpreter's own TCE is wider — and dead code on this path

`try_stackless_invoke` in `vm/src/runtime/interpreter/invoke.rs` carries its own
self-tail-call elimination, with the fidelity argument written out in full
(it names `ControlFlowPointcutTests` as the casualty that forced it to be
restricted to self-recursive calls). Its opcode window is **wider** than the
compiler's — it maps a `V` return type to 0xb1 and accepts it.

It never runs. `Frame::reset_for_tail_call` has exactly one production caller,
that step-8 block, and the cached `invokestatic` path never reaches it:
`dispatch_static::execute_invokestatic_cached` builds the frame itself
(`Frame::new_pooled_cached`) and pushes it. A census of `CRATONVM_FRAME_TRACE=1`
over `--nojit SWFrames`, counting every frame event naming `recurse`:

```
   2080 [FRAME_POP]
      1 [FRAME_PUSH/stackless]            <- the one uncached push
   2079 [FRAME_PUSH/stackless_cached]     <- every self-call
      0 [FRAME_TCO]
```

32 walks x 65 activations = 2080. One push consulted the TCE; 2079 could not.
That is why `--nojit` keeps 68 frames and overflows at 4M, and it means the
interpreter's elimination — written specifically for deep self-recursion — has
been inert for as long as the invoke cache has served that shape.

## What is NOT established

- **Whether the interpreter's TCE should be revived or deleted.** It is dead
  code today. Reviving it on the cached path would reproduce this defect in the
  interpreter; deleting it would remove the fidelity argument's only home. The
  `s16_tail_call_sum_10000` test that depends on it should be checked for
  whether it exercises the cached path at all.
- **Whether any workload method lands in the permanent case.** The census to
  run: self-recursive `invokestatic` sites whose `pc+3` is `0xac..=0xb0`,
  outside any handler range, in a method with a `new`/`anewarray`/`indy` (which
  is what makes `c2_upgrade_would_engage` refuse it). Now cheap to run — flip
  `CRATONVM_JIT_SELF_TAILCALL` over a suite and diff the failures.
- **The fix.** Three shapes were considered and none is free: delete the arm
  (costs 5.5x on the permanent population); keep the `JMP` but carry a synthetic
  depth counter so `StackOverflowError` still fires (does nothing for the stack
  walks); keep it only for methods the supersede will claim (buys nothing —
  those are the ones it is worth nothing on). Defaulting the new switch OFF is
  the one option that restores HotSpot semantics outright, at a cost that is
  zero for the superseded population and 5.5x for the other.

## Reproduction

```bash
javac -d /tmp/swprobes probes/SWTce.java probes/SWTce3.java probes/SWTce4.java \
                       probes/SWFrames.java probes/SWShape.java \
                       probes/SWTceBench.java probes/SWTceBench2.java
V="./target/release/cratonvm --java-home <jdk25> -cp /tmp/swprobes"

# the permanent case, and the switch that fixes it
$V cratonvm.SWTce                                    # RETURNED
CRATONVM_JIT_SELF_TAILCALL=0 $V cratonvm.SWTce       # STACK_OVERFLOW

# the shape gate, C1 pinned so the tier cannot mask it
CRATONVM_TIER_C2_THRESHOLD=100000000 $V cratonvm.SWTce3

# the frame-count symptom, deterministic; then isolated to the lowering
CRATONVM_C2_SUPERSEDE=0 $V cratonvm.SWFrames                                  # n=4
CRATONVM_C2_SUPERSEDE=0 CRATONVM_JIT_SELF_TAILCALL=0 $V cratonvm.SWFrames     # n=68

# what it costs, interleaved, engagement printed beside the number
for f in 1 0; do CRATONVM_JIT_SELF_TAILCALL=$f $V cratonvm.SWTceBench2 1000 200000; done

# controls
$V --nojit cratonvm.SWTce3
<jdk25>/bin/java -cp /tmp/swprobes cratonvm.SWTce3
```

Expected on a fixed VM: `STACK_OVERFLOW` in every arm of `SWTce` and `SWTce3`,
and `SWFrames` holding `n=68` on every walk of every run, with or without any
tier lever.

## Related

- `fixed-suite-bugs/jit/jit-self-recursive-activations-invisible-to-stack-walks-FIXED-20260819.md`
  — the stack-walking defects this was split out of, all fixed.
- `jit/src/x64/bytecode_walk.rs` — the tail-JMP arm itself.
- `jit/src/x64/licm.rs` — `self_tailcall_enabled` (this defect's switch) and
  `sp_tailcall_enabled` (the sibling tail-call's).
- `jit/src/lib.rs` — `invokestatic_self_call_uses_tail_jump` (the opcode
  window), `scalar_selfrec_ir_would_engage` (the `(I)I` early promotion),
  `c2_upgrade_would_engage` (the supersede admission that refuses allocators).
- `jit/src/tiered.rs` — `CompilerCore::request_c2_upgrade` and
  `c2_upgrade_gate`, the unlogged C2 door.
- `vm/src/runtime/interpreter/invoke.rs` — the interpreter's own TCE and the
  fidelity argument that restricts it.
- `vm/src/runtime/interpreter/dispatch_static.rs` —
  `execute_invokestatic_cached`, the path that makes that TCE unreachable.
- `jit/src/ir_lower.rs` — `emit_self_recursive_call`. Recorded here previously
  as "the obvious suspect", REFUTED for the older frame-collapse defect. The
  refutation was right twice over: it is not this defect's emitter either, and
  it is in fact the lowering that keeps the frames.
