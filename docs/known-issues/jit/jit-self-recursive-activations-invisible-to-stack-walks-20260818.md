# A JIT-compiled method's self-recursive activations are invisible to every Java stack walk

**Status: OPEN, reproduced and isolated 2026-08-18 on `dev` `a392c9ded`.
Once a directly self-recursive method tiers up, 64 nested activations report as
ONE frame to both `Throwable.getStackTrace()` and `StackWalker`. The computed
answers stay correct — this is a stack-VISIBILITY defect, not a miscompile.
HotSpot 25 and `--nojit` keep all 67 frames on the same probes.**

## The failure

`vm/tests/wp1_9_stackwalker.rs::stackwalker_log4j_deep_repeated_walks_finish_under_jit`
fails on `dev`, and not for the reason its name suggests — nothing times out:

```
AssertionError: expected cratonvm.StackWalkerLog4jStressProbe$LoggerFactory
                but got cratonvm.StackWalkerLog4jStressProbe
```

That probe reproduces Log4j2's caller-class lookup: walk the stack, drop the
`Locator` frames, take the first frame in the owning package. The answer is
wrong because `LoggerFactory.resolveCaller`'s frame is simply **not in the
walk** once the enclosing recursion is compiled — so the first surviving
package frame is `recurse`, declared in the outer class.

## Measured

Azure Linux, `dev` `a392c9ded`, one release binary, real JDK 25 backend.
Probes are committed under `probes/` (`SWFrames`, `SWThrow`, `SWShape`,
`SWMutual`, `SWValue`); each does 32-60 rounds so the first rounds are
interpreted and the later ones compiled.

### The frame count, before and after tier-up

`StackWalker.walk` and `Throwable.getStackTrace()` over a 64-deep direct
self-recursion, same process, early round vs late round:

| arm | early round | late round |
|---|---:|---:|
| CratonVM, JIT on | 67-68 | **3** |
| CratonVM, `--nojit` | 67-68 | 67-68 |
| HotSpot 25 | 67-68 | 67-68 |

65 of 68 frames disappear, and they disappear at the moment the method is
compiled — not gradually.

### Which call shapes lose frames

One process, one binary, three shapes measured side by side (`SWShape`,
`SWMutual`):

| shape | depth | early | late (JIT) | `--nojit` | HotSpot |
|---|---:|---:|---:|---:|---:|
| direct self-recursion, TAIL call | 64 | 67 | **3** | 67 | 67 |
| direct self-recursion, NON-tail | 64 | 67 | **3** | 67 | 67 |
| mutual recursion `a→b→a→b` | 64 | 67 | 67 | 67 | 67 |
| distinct-method chain `c0→…→c7` | 8 | 10 | 10 | 10 | 10 |

**Only DIRECT self-recursion collapses.** Mutual recursion — same depth, same
heat, no two consecutive identical frames — keeps every frame, which rules out
"consecutive duplicate frames are de-duplicated" and rules out "one entry per
distinct method on the stack" (that would report 2 for the mutual case, and it
reports 67).

### The answers are still right

`SWValue` runs non-tail self-recursion whose caller does arithmetic on the
returned value (`sum(100)`, `fact(12)`, `depthCount(64)`), 60 rounds, well past
tier-up: `wrongRounds=0/60` under JIT, `--nojit` and HotSpot alike. So the
compiled activations genuinely exist and unwind correctly; only the *record* of
them is missing.

## Mechanism

`runtime/stackwalker.rs::capture_full_trace` builds the Java-visible stack from
the interpreter's `thread.frames`, then splices in
`jit::conservative_roots::active_compiled_frames()`. Its own doc comment states
the invariant this defect violates:

> A JIT-compiled method runs without pushing a `Frame`, so on its own `frames`
> is the Java stack *minus everything the JIT has taken over*.

`active_compiled_frames` reads `JIT_ENTRY_CHAIN`, which holds **one entry per
interpreter→JIT entry**. A compiled method that calls *itself* from inside
compiled code never re-enters the JIT from the interpreter, so it pushes no
chain entry — 64 activations, one entry. A compiled method calling a
*different* compiled method evidently does push one (the mutual-recursion row
keeps its frames), which is why the defect is specific to direct self-calls.

## What is NOT established

- **Which emitter is responsible.** The obvious suspect,
  `ir_lower.rs::emit_self_recursive_call` (a direct `CALL rel32` to the
  method's own entry, bypassing `jit_invoke_dispatch`), is **REFUTED**:
  `CRATONVM_JIT_IR_SELFREC_DIRECT=0` changes nothing, measured in one binary,
  both arms. Some other self-call path is the one that skips the chain push.
- **Whether it is a regression or newly exposed.** The test passed on a
  dev-baseline log from 01:43 the same day, so *something* changed today that
  made these methods compile where they previously did not. The chain-entry gap
  itself may be much older; no bisect was run.
- **Blast radius beyond stack walks.** Anything keyed on frame identity is
  suspect — `Reflection.getCallerClass`, security/caller-sensitive checks,
  Spring's `ControlFlowPointcut`, and `StackOverflowError` depth accounting were
  not measured. Note `invoke.rs`'s tail-call-elimination comment already records
  one such casualty (`ControlFlowPointcutTests`) for the *interpreter* TCE and
  restricts TCE to self-recursive calls for exactly this reason; the JIT is now
  reintroducing the same invisibility on the compiled path, for non-tail calls
  too.

## Reproduction

```bash
javac -d /tmp/swprobes probes/SWShape.java probes/SWMutual.java probes/SWValue.java
./target/release/cratonvm --java-home <jdk25> -cp /tmp/swprobes cratonvm.SWShape
./target/release/cratonvm --java-home <jdk25> --nojit -cp /tmp/swprobes cratonvm.SWShape
<jdk25>/bin/java -cp /tmp/swprobes cratonvm.SWShape
```

Expected on a fixed VM: `tail`, `nonTail` and `chain` hold their early-round
counts in every arm, matching HotSpot.

The failing suite test is

```bash
cargo test -p cratonvm-vm --release --test wp1_9_stackwalker \
  stackwalker_log4j_deep_repeated_walks_finish_under_jit
```

## Related

- `vm/src/runtime/stackwalker.rs` — `capture_full_trace` /
  `interleave_compiled_frames`, and the `interp_depth` splice contract.
- `vm/src/jit/conservative_roots.rs` — `JIT_ENTRY_CHAIN`,
  `active_compiled_frames`.
- `vm/src/runtime/interpreter/invoke.rs` — the interpreter's own tail-call
  elimination, already restricted to self-recursive calls on stack-trace
  fidelity grounds.
