# An inline splice's return value can land on an ENCLOSING spliced callee's live local

| | |
|---|---|
| **Status** | OPEN, and the leading hypothesis is now REFUTED — see *The recommended experiment was run, and it says no*. The title names a mechanism that accounts for 3 of 304 reports, not the 151. |
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

## The recommended experiment was run, and it says no

This page recommended option 2 — *"extending the clamp to include every open
scope's locals region (not just the operand stack) is a two-line change, and
running the affected workload with it on and off says whether any of these 151
was load-bearing."* Done, 2026-09-07. **Nothing moved.**

The guard was written exactly as described: `caller_post_pop_spill` clamped up
to the top of every ENCLOSING open scope's locals (excluding the splice's own,
whose locals are dead at its `xreturn`), behind
`CRATONVM_JIT_NO_INLINE_ENCLOSING_LOCAL_CLAMP` with its own engagement counter.
`CriteriaWindowFunctionTest`, same binary, one switch:

| arm | engagements | overlap reports | ENCLOSING | INNERMOST | result |
|---|---:|---:|---:|---:|---|
| clamp ON | **3** | 304 | **151** | 152 | 11/11 |
| clamp OFF | 0 | 304 | **151** | 152 | 11/11 |

The guard engages (3, and the switch gates it cleanly to 0) and changes the
census by **zero**. So the mechanism this page's title names — the `xreturn`
cursor rewind landing a return value on an enclosing callee's live locals — is
real but accounts for 3 reservations, not for the 151 reports.

**The change was reverted.** It is a behaviour change to working code with no
witness of harm, and its one validation came back null; shipping after that is
worse than shipping without it. The two-line diff is recoverable from this
page's description if a witness ever appears.

## What the 151 actually are, and what that rules out

With the labels the detector now carries, the two populations are structurally
different — they are not the same event seen twice:

* **INNERMOST, 152 reports** — all `scope #0/1`: a single open scope, the
  splice that is returning, its locals dead. Harmless, as this page said.
* **ENCLOSING, 151 reports** — all `scope #1/3`: **three** scopes open, and the
  reservation lands exactly on the MIDDLE one's locals
  (`reservation 160..168` against `locals 160..168 (num_locals=1)`, and five
  other offsets of the same shape).

They are not paired reports of one reservation, which was the other candidate
explanation and is now excluded.

That `#1/3` shape is what rules the return-value push out. The clamp is computed
at splice start from `inline_oop_scopes`, and for the innermost splice of a
three-deep nest it raises the cursor above scope #1's locals — so if these
reservations came from `caller_post_pop_spill` they would have moved. They did
not. Something else writes `next_spill_offset` low enough to hand out scope #1's
locals while scope #2 is still open.

### Where to look next

`next_spill_offset` has ~20 writers in `x64/inlining.rs`. All but three are the
bail path (`= callee_local_base`, unwinding a refused splice). The three that
are not:

| site | what it does |
|---|---|
| `= spill_checkpoint` (~1548) | full rollback of an abandoned splice |
| `= save_spill` (~1927) | merge-point restore inside the walk |
| `= caller_post_pop_spill` (the `xreturn` arms) | **excluded by the experiment above** |

The merge-point restore is the interesting one: `save_spill` is captured after
the merge region is reserved, so it should sit above this splice's own locals —
but it is captured ONCE per splice and restored at every merge point, and a
nested splice that has since moved the cursor is exactly the shape the `#1/3`
reports have. That is a reading, not a measurement, and this page has just
demonstrated the cost of acting on one of those.

The cheap instrument is the one this page already suggested as option 1 and
which is now the ONLY option left standing: have `dbg_note_spill_overlap` also
report which reservation site it came from (`SpillReason` is already passed in
and is `Push` for all 304 — the caller of `push_stack` is what is missing), and
whether the overlapped local is still read at or after the enclosing scope's
`cur_pc`. That turns 151 "maybe" into a named writer and a liveness answer.

## Related


* `docs/internal/fixed-bugs/jit-warm-groupdata-window-row-collapse-20260906-FIXED.md`
  — where the count came from, and the defect it was NOT.
* The `LEATest` miscompile recorded in `try_emit_inline_body`'s own comments —
  the same class of defect on the operand stack, fixed by the clamp this page
  originally proposed extending to locals. That extension is now tried and
  refuted as an explanation for the 151; see above.

## A note on the title

It names the mechanism this page was opened on, and that mechanism is now known
to account for 3 reservations rather than 151. The title is kept so a search for
it still lands, in the same spirit as the phi-home page's own retitling note —
but read *The recommended experiment was run* before acting on it.
