# An inline splice's return value can land on an ENCLOSING spliced callee's live local

| | |
|---|---|
| **Status** | OPEN. The hazard is real and now quantified; **not observed to produce a wrong answer** on the workload that exposes it. |
| **Origin** | The residual `jit-warm-groupdata-window-row-collapse-20260906` left as "that hazard is real and deserves its own page". This is that page — with the count classified rather than repeated. |

## What the earlier count actually was

`jit-warm-groupdata-window-row-collapse-20260906-FIXED.md` reported:

> `Compiler::dbg_note_spill_overlap` — a spill reservation that hands out a
> frame slot an OPEN inline scope still owns. It fires **325 times** on this
> workload (all `Push` reservations onto a `num_locals=1` scope, mostly under
> `net/bytebuddy/...`), so that hazard is real and deserves its own page — but
> none of the reports is `Select.processGroupResult`.

Every word of that is true and none of it is actionable, because the detector
could not say **which** open scope it had hit — and that is the whole
difference between a coincidence and a miscompile:

* the **INNERMOST** scope is the splice that is RETURNING. Its `xreturn` arm
  has already loaded the value into RAX, so that callee's locals are dead at
  that instruction, and the wrapper pops the scope a few lines later. Landing
  the return value on its own local 0 is harmless.
* an **ENCLOSING** scope's body CONTINUES after the inner call returns. Its
  locals are live, and a reservation handed out inside them is a second owner
  for a word the enclosing callee still reads.

`dbg_note_spill_overlap` now says which (`#{depth}/{len}` plus the word), so
this question does not have to be re-opened from a bare count again.

## The measurement

`CriteriaWindowFunctionTest` on the hib-suite fixture,
`CRATONVM_DBG_JIT_SLOT_OVERLAP=1`, 2026-09-07, `dev` + the deopt-sink fix:

| | |
|---|---:|
| reports | **308** |
| ...INNERMOST (harmless) | 157 |
| ...**ENCLOSING (the hazard)** | **151** |
| reservation reasons | `Push`, 308 of 308 |
| distinct methods with an ENCLOSING report | 44 |
| `@@RESULT` for the same run | `found=11 started=11 ok=11 failed=0` |

So it is not, as the shape of the original count suggested, all one benign
thing. **About half the reports are against a scope whose locals are live.**

Depth distribution of the ENCLOSING half: 147 are `scope #1/3` and 4 are
`scope #0/2` — i.e. they are middle scopes in a nested splice, never the
outermost method's own frame.

Population, by owner of the compiled method:

* `net/bytebuddy/description/...` — 39 of the 44 methods (`ParameterList`,
  `TypeList$Generic`, `TypeDescription`, `MethodList`), the reflection-to-model
  layer Hibernate's enhancer drives;
* `org/h2/result/RowFactory$DefaultRowFactory.createRow`,
  `org/h2/mvstore/tx/Transaction.closeIt`,
  `java/util/regex/Pattern.compile` / `.newSlice`,
  `org/hibernate/bytecode/enhance/internal/bytebuddy/GetPropertyValues.apply`.

`Select.processGroupResult` is still absent, and the two `Select` reports in
the run (`Select$LazyResultSelect.<init>`) are both INNERMOST. The earlier
page's conclusion — that this is a different defect from the phi-home one it
was fixing — stands.

## The mechanism, read from source

`try_emit_inline_body` (`jit/src/x64/inlining.rs`) reclaims the callee's frame
at every `xreturn` by rewinding the spill cursor and pushing the result:

```rust
self.next_spill_offset = caller_post_pop_spill;
self.push_from_rax();
```

with this justification beside it:

> Safe: the load above already read the value out of the callee's slot, and
> `caller_post_pop_spill` is **strictly below** `callee_local_base`, so the
> store cannot alias anything the callee still owns.

Two things are wrong with that sentence as a safety argument.

**It is not always strictly below.** `caller_post_pop_spill` starts at the
cursor before this splice's reservation and is lowered by `min` over the popped
arguments' frame slots — but the live-slot clamp then raises it back to
`live_top`, the top of the caller's remaining live `Frame` operands, whose
maximum is exactly `callee_local_base`. Every INNERMOST report is that equality.

**"anything the callee still owns" is the wrong scope.** The claim is about the
RETURNING callee. In a nested splice the cursor also has to stay above the
locals of every ENCLOSING spliced callee, and nothing checks that: the live-slot
clamp (added for the bc-java `LEATest` miscompile, `iinc` re-slotting an index
across two splices) scans `self.stack` — the symbolic OPERAND stack. An
enclosing splice's LOCALS are not on it. `dbg_note_spill_overlap`'s own doc says
this in as many words; what it could not say was how often it happens, and the
answer is 151 times in a 22-second run.

## Why nothing is visibly broken

The same run is 11/11. Two reasons it can be quiet, and neither is a defence:

* the clobbered local may be dead from that point in the enclosing body (the
  common case for a `num_locals=1` scope whose single local is `this`, already
  copied into a register);
* the value written may be the same object the local held, when the inner call
  returns its own receiver — very common in bytebuddy's `describe`/`of`/`wrap`
  chains, which is exactly where 39 of the 44 methods come from.

Neither is enforced anywhere, so both are luck.

## What would settle it

Not another census. What is missing is a case where the enclosing callee READS
the clobbered local after the inner call, with a value that differs. Two ways
in:

1. **Make the detector prove liveness.** `InlineOopScope` already carries the
   per-pc local oop masks (`masks`, `reached`) computed by
   `compute_local_oop_masks`. A report could say whether the overlapped local
   is still read at or after the enclosing scope's `cur_pc` — turning 151
   "maybe" into a number of "definitely".
2. **A/B the clamp.** `inline_live_slot_clamp_disabled()` already exists.
   Extending the clamp to include every open scope's locals region (not just
   the operand stack) is a two-line change, and running the affected workload
   with it on and off says whether any of these 151 was load-bearing. If
   nothing moves, the cost is a slightly higher frame; if something moves, the
   miscompile has a witness.

Option 2 is the cheaper experiment and is the recommended next step. It was not
taken here because it is a behaviour change to the inliner, on a workload that
is currently green, in a session whose subject was a different defect — and
this repository's own rule for that situation is a page, not a patch.

## Related

* `docs/internal/fixed-bugs/jit-warm-groupdata-window-row-collapse-20260906-FIXED.md`
  — where the count came from, and the defect it was NOT.
* The `LEATest` miscompile recorded in `try_emit_inline_body`'s own comments —
  the same class of defect on the operand stack, fixed by the clamp this page
  says is incomplete for locals.
