# COV-07 — `athrow` refuses 89 methods, and it is the one refusal that may be right

**Status:** not started. **Do this one last, and be willing to close it as
"keep the refusal".** **Owns:** the `scan.has_athrow` conjunct of
`ir_compatible` (`jit/src/ir.rs:6073`) and nothing else until the question
below is answered. **One conjunct** — `cov-05` and `cov-06` own the others.

## The measurement

**89 events.** Third-largest whole-method refusal, behind `checkcast` (306) and
`anewarray` (138), and unlike either of those it is a *control-flow* refusal
rather than a missing lowering.

## Why this lane is framed as a question

The other `cov-*` lanes all have the same shape: the single-pass backend does
this, the IR tier does not, close the gap. `athrow` is not that shape.

`athrow`'s single-pass lowering bakes a bci as an immediate and hands it to
`jit_throw_exception`, which `route_jit_exception_through_method` range-tests
against the method's own exception table. That is one of the **four** bci-baking
sites this project has already had to reason about carefully — see
`docs/jit/loop-rewriter-wiring.md`'s coordinate-change table, where the `athrow`
site is explicitly the one that "is not in `loop-transform-wiring.md`'s list; it
was found by grepping every `pc as i32` immediate in the backend rather than by
reading that list."

So the question this lane answers first is not "how do I lower `athrow`" but:

**Can the IR tier express a throw whose handler resolution happens in the
interpreter, and can it do so without inventing a second, divergent answer to
where an exception goes?**

If the answer needs the IR lowerer to model exception edges into handler blocks,
that is a substantially larger piece of work than the other six `cov-*` lanes
put together, and 89 events is not obviously worth it. Deciding **not** to do it
— with the reasoning written down and the conjunct left in place — is a
legitimate and probably the correct outcome. Say which, explicitly.

## What to establish before writing any code

1. **Where do the 89 come from?** `CRATONVM_DBG=ir-compiles` prints the method.
   Group them: an explicit `throw` in application logic is a different
   proposition from a `throw` in a rarely-taken validation branch of an
   otherwise hot method. If they are overwhelmingly the second, the value is in
   the *rest* of those method bodies and an "outline the throw" treatment may be
   worth more than lowering it.
2. **Does `checkcast` need this too?** `cov-05`'s second increment lowers
   `checkcast`, which throws on failure. If that routes through the same
   machinery, the two lanes are not independent and `cov-05` inherits this
   question. Establish it before either lane commits.
3. **What does `precise_exception_frames` do here?** It is already an admission
   term in its own right (`"precise exception frames required (RBC.6: a handler
   reads a non-parameter local)"`), and it is a *separate* refusal from
   `has_athrow`. Two exception-shaped gates that refuse different things is
   exactly the sort of overlap that gets one of them silently widened.

## How to verify — if it proceeds

* The `has_athrow` conjunct count falls and `admitted` rises by the same amount.
* `jit/tests/ir_vs_singlepass.rs`: a method that throws and catches locally;
  a method that throws past its own handler; and a `try`/`finally` where the
  `finally` must run on **every** escape route. That last one is not
  hypothetical — this VM has shipped a JIT-compiled `finally` that was skipped
  on three escape routes (`probes/FinallyBalanceProbe.java` is the witness), and
  a second lowering for `athrow` is a second chance to ship it.
* The thrown exception's **bci** must match the single-pass backend's, because
  that is what the handler range test consumes.

## What to refuse

Everything, until question 1 is answered. This is the lane most likely to be
closed as "the refusal stays", and that is a result — the survey's job was to
say which refusals cost something, not which are worth removing.
