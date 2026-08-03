# COV-04 — the largest structural refusal is an `invokespecial` the builder will not model

**Status:** not started. **Independent of every other lane.**
**Owns:** the `0xb7` / `0xb9` arms and the `<init>`-elision path of
`IrBuilder::build` in `jit/src/ir.rs` (`ir.rs:5152` onward, and the bail sites
at 5204, 5219, 5264, 5314). Not `ir_compatible`, not `plan_inline`.

## The measurement

69 events, the largest structural bucket:

| site | what it is | events |
|---|---|---:|
| `ir.rs:5204` | `invokespecial` that is neither a resolvable call nor a trivial `<init>` this pass may elide | 53 |
| `ir.rs:5264` | an invoke with **no `invoke_info` entry at that pc** | 13 |
| `ir.rs:5314` | — (read the site; 2 events) | 2 |
| `ir.rs:5219` | — (read the site; 1 event) | 1 |

## Read `ir.rs:5204` before planning anything

The bail sits on the *else* of "was this call resolvable", inside the
scalar-new `<init>` elision path, and it refuses when
`!self.trivial_init_pcs.contains(&pc)`. So 53 of these are not "an opcode is
missing" — they are "this `invokespecial` is a real call and the builder's
only other option here is to elide it, which it may not".

That means the honest first question is **which** of the two it is:

* a superclass or private `invokespecial` that *should* lower to an ordinary
  call and the arm simply does not have that path; or
* a constructor the elision analysis correctly declines, where the right answer
  is a real call to `<init>` and the builder has no way to emit one.

Answer it by grouping the 53 by callee before writing code. `CRATONVM_DBG=ir-compiles`
prints the method the bail came from; the callee is one `javap` away. **Do not
size this lane before that grouping exists** — the two cases have completely
different work behind them, and this doc deliberately does not guess the split.

`ir.rs:5264` is different and much simpler: the caller supplied no
`invoke_info` for that pc, so the site was never resolved at compile time. The
single-pass backend treats that as "bail the method" too (see the `0xba` arm's
`return false` on an unresolvable indy). Whether 13 events is worth a deferred
path is a judgement, but the *finding* — that the two backends already agree
here — should be stated rather than rediscovered.

## The first increment

The grouping above, written into this doc, and nothing else. This is the one
lane in the `cov-*` set whose work cannot be estimated from the survey, and
starting it with code is how a lane spends a week on the smaller half.

After that, whichever of the two cases is larger, as its own increment.

## How to verify

* `CRATONVM_DBG=ir-compiles`: `refused at ir.rs:5204` falls and
  `optimizing backend produced a body` rises. As everywhere in this set, quote
  both — a method that stops failing here and immediately fails on the next
  unlowered opcode is a real outcome and it is not a body.
* `jit/tests/ir_vs_singlepass.rs` for each shape the increment admits.
* An `<init>` that this lane starts *calling* rather than eliding must still
  run every side effect the elision path was allowed to skip. The existing
  defence-in-depth comment at the elision site ("eliding a `<init>` whose
  receiver is `this` or a parameter would skip a real superclass constructor
  and hide any escape it performs") is the hazard, from the other direction.

## What to refuse

Any `invokespecial` whose callee identity is not established at compile time,
and any elision this lane cannot prove is on a fresh `Op::New` it emitted
itself. Both are refused today. The lane makes the refusal narrower or it does
nothing; it never makes it a guess.
