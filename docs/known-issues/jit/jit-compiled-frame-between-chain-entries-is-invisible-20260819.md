# A compiled frame BETWEEN two JIT chain entries is invisible to every Java stack walk

**Status: OPEN, isolated 2026-08-19 on `dev`. This is the last of four
mechanisms behind one symptom, and the only one still unfixed. It is what keeps
`stackwalker_log4j_deep_repeated_walks_finish_under_jit` red.**

## The failure

Log4j2's caller lookup answers with the enclosing class:

```
AssertionError: expected cratonvm.StackWalkerLog4jStressProbe$LoggerFactory
                but got cratonvm.StackWalkerLog4jStressProbe
```

`probes/SWStress3.java` instruments the failing walk itself — same call path,
same heat — and shows what the walk saw:

```
got=cratonvm.SWStress3   saw=Locator.getCallerClass | recurse | recurse | recurse | …
```

`LoggerFactory.resolveCaller` belongs between `getCallerClass` and `recurse` on
the real stack. It is absent, so the `dropWhile` chain runs past it and returns
the outer class.

## Mechanism

`resolveCaller` is a fully compiled method with a real frame. It has no
`JIT_ENTRY_CHAIN` entry because compiled code called it **directly**, and the
per-entry RBP walk never reaches it either:

* `getCallerClass` owns the innermost chain entry. Its `[rbp+8]` is a non-JIT
  return address (it was entered from the VM), so the walk that starts at that
  entry's `exact_rbp` stops immediately — `nested=1`.
* `resolveCaller` sits one frame further out, entered by a direct
  compiled→compiled call from `recurse`, so it pushes no entry of its own.
* `recurse`'s own chain entries start at their own frames and walk OUTWARD, so
  none of them descends to `resolveCaller` either.

It falls in the gap between two chain entries, and `active_compiled_frames`
walks a band per entry rather than the stack as a whole.

## What it is NOT — three refuted hypotheses, each measured

Recorded because each cost real time, and two of them look right from the code.

* **Not inlining.** `CRATONVM_DBG_JITC` says
  `inline-resolve REFUSED cratonvm/SWCross.helper()V depth=0:
  new/anewarray/multianewarray`, and the Log4j shape has **zero**
  `inline-splice` lines in the whole run while `resolveCaller` is
  `full-compile`d standalone and `bg-direct-call BOUND`. Also:
  `IrBuilder::build` does not inline at all — `Lowerer::inline_scopes` is empty
  on every compile, per its own doc comment.
* **Not the self-recursion collapse.** Fixed 2026-08-18 by the saved-RBP walk
  in `active_compiled_frames`; `SWShape` holds 67/67 and `SWMutual` 67 across
  tier-up.
* **Not the compile-id misnaming.** Fixed 2026-08-19: the single-pass backend
  never assigned `cm.compile_id`, so `bind_compile_id` bound 0 and every
  fast-tier innermost frame wore its caller's name. `SWFrames` went to
  HotSpot parity (n=68, `hasLoggerFactory=true`) — but the Log4j probe did not,
  which is what isolated the mechanism above.

## Where to start

One walk over the whole native stack instead of a band per chain entry: begin
at the innermost entry's `exact_rbp` and keep following saved-RBP links across
the non-JIT boundaries, using each chain entry's `entry_sp` only to attribute
`interp_depth`, not to bound the walk. The bounds and validation predicates in
`remap_active_jit_frames`' Stage 5 are the ones to copy; the read is
read-only, unlike that one.

Re-measure before assuming anything here. Every hypothesis in this file that
was not measured turned out to be wrong.

## Reproduction

```bash
javac -d /tmp/pb probes/SWStress3.java
CRATONVM_JIT_THRESHOLD=1 ./target/release/cratonvm --java-home <jdk25> \
  -cp /tmp/pb cratonvm.SWStress3
```

Expected on a fixed VM: `bad=0/32`, `STRESS3_OK`, matching HotSpot.

The suite test is

```bash
cargo test -p cratonvm-vm --release --test wp1_9_stackwalker \
  stackwalker_log4j_deep_repeated_walks_finish_under_jit
```

## Related

- retired/jit-self-recursive-activations-invisible-to-stack-walks-RETIRED-20260819.md
  — the page this was found under. Its own defect (self-recursive activations
  collapsing to one frame) is fixed; this is what remained.
- `vm/src/jit/conservative_roots.rs` — `JIT_ENTRY_CHAIN`,
  `active_compiled_frames`, `innermost_frame_method`.
- `vm/src/runtime/stackwalker.rs` — `capture_full_trace`,
  `interleave_compiled_frames`, `drop_osr_continuations`.
