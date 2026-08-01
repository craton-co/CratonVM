# Loop transforms: peeling and unrolling at the bytecode level

Status: implemented in `jit/src/x64/licm.rs` (analysis + rewriter + tests);
**not yet wired into `compile_with_param_slots`** — see "Wiring" below.

## Why bytecode-to-bytecode

The x86-64 backend already unrolls, but it does it by *byte-copying emitted
machine code* at the back edge (`jit/src/x64.rs`, the `0xa7` arm around
L15605-L15960). That duplicator has to shift, per copy, every one of eight
patch vectors, re-resolve helper `rel32`s and mint fresh IC slots; each new
patch vector is a new way for it to go stale. It also decides what to unroll
from a raw body-byte-size heuristic (`x64.rs` L23561-L23596) that consults
none of the structural facts the LICM/BCE passes right below it consult.

Rewriting the *bytecode* instead means the whole existing pipeline —
`detect_loops`, `find_bypassable_loop_headers`, arith/FP LICM, speculative
BCE, the SIMD preheaders, regalloc — re-runs on the transformed method and
sees one consistent CFG. There is nothing to keep in sync, because there is
only one representation.

## The two transforms are one rewriter

```text
    original          peel(k)                unroll(k)
    ────────          ───────                ─────────
    H: body           H: body    (copy 0)    H: body    (copy 0)
       goto H            …                      …
                         body    (copy k-1)     body    (copy k-1)
                      S: body    (copy k)    S: body    (copy k)
                         goto S                 goto H
```

The two outputs are byte-identical except for the back edge's target: peel
points it at the last copy (so copies `0..k-1` run once), unroll points it at
the first (so all `k+1` run every trip). One rewriter, one refusal set, one
provenance map.

Every copy carries the body's own exit branches, so there is **no trip-count
precondition**. Trip counts 0 and 1 are not special cases: with 0 the first
copy's exit test fires before any body effect, which is the instruction the
original would have executed anyway.

## Preconditions

All of these are refusals (`LoopXformRefusal`), never fallbacks:

| Condition | Refusal |
|---|---|
| back edge is an unconditional `goto` to the header | `ConditionalBackEdge`, `NotABackEdge` |
| header dominates every instruction in `[header, back_edge_end)` | `Irreducible` |
| no branch from outside the region lands below the header | `ExternalEntry` |
| every cycle strictly inside the body is reducible | `IrreducibleInnerLoop` |
| every region instruction is reachable | `UnreachableInRegion` |
| no `jsr`/`ret`/`jsr_w`/`goto_w` in the method | `OpaqueControlFlow` |
| no `tableswitch`/`lookupswitch` in the method | `SwitchInMethod` |
| nothing branches to the back-edge instruction | `BranchToBackEdge` |
| no handler in the region, no partially overlapping range | `HandlerInRegion`, `HandlerRangeStraddlesRegion` |
| rewritten offsets still fit the 2-byte signed field | `OffsetOverflow` |
| body and copy count within their caps | `BodyTooLarge`, `TooManyCopies` |
| poll-free span within budget | `TimeToSafepointBudget` |

Reducibility is answered with real dominators (Cooper/Harvey/Kennedy over an
instruction-granularity CFG, `MethodCfg`), not with the "any backward branch
is a loop" heuristic the rest of the backend uses. `MethodCfg::build` returns
`None` — a refusal — on a length-table desync, a mid-instruction branch
target, opaque control flow, or a dominator fixpoint that does not settle.

Two limitations are conservative rather than fundamental, and both are cheap
to lift later: switches are refused because their 4-byte operand padding
depends on their own PC (shifting them changes their *length*), and a
`do { } while` back edge is refused because its exit test would have to be
replicated at the end of every copy.

## Safepoint polls

The emitter's rule is PC-local: at `ifeq..if_acmpne` (`0x99..=0xa6`), `goto`
(`0xa7`), `tableswitch`/`lookupswitch` (`0xaa`/`0xab`), `ifnull`/`ifnonnull`
(`0xc6`/`0xc7`), if any decoded target is `<= pc` it emits the poll *before*
the compare, so the poll runs whether or not the branch is taken.
`poll_bearing_opcode` is that opcode set transcribed. `goto_w` is absent from
it — it is rejected by `jit_scan` today, and it is the one branch that could
close a cycle without a poll.

That yields a structural theorem:

> Every fall-through edge strictly increases the PC, so every cycle in a
> linear bytecode CFG contains at least one edge whose target is `<=` its
> source. If every such backward branch sits at a poll-bearing opcode, every
> cycle is polled.

`all_backward_edges_are_polled` checks the antecedent, and the rewriter runs
it **on its own output** before returning: a transform that produced a
poll-free cycle refuses instead of publishing itself. The check is not
vacuous — it fails on a backward `goto_w`, on `jsr`/`ret`, and on malformed
branches, all of which are tested.

Per transform:

* **Peel** leaves the loop alone. The steady-state copy keeps the same back
  edge, so it still polls once per iteration; the peeled copies run once and
  extend the one-shot span between the method-entry poll and the first
  back-edge poll by `k * body_len` bytecodes.
* **Unroll** keeps one back edge for `k+1` bodies, so steady-state
  time-to-safepoint grows by the unroll factor. It stays *bounded* because
  both factors are bounded (`body_len <= 256`, `k+1 <= 8`) and the product is
  re-checked per call against `LOOP_XFORM_MAX_POLL_FREE_BYTES`.

The tests assert this dynamically as well as statically: for a loop running
`n` iterations, peel executes exactly `max(n - k, 0)` polls and unroll
exactly `floor(n / (k+1))` — so the poll count never falls below one per
`k+1` iterations, which is what "time to safepoint stays bounded" means.

## Deopt, OSR, provenance

`LoopXform::bci_of` maps **every byte** of the output to the original bci it
was copied from. The rewriter copies bytes and rewrites branch *offsets*
only, so no local index and no operand-stack shape is renamed or reordered,
and the copies are laid out in execution order. Therefore the interpreter
state at output PC `p` is exactly the state the original method had at
`bci_of[p]`, and a deopt maps through `bci_of` with the frame already
correct.

The tests assert the strong form: the entire `(bci, locals)` step sequence of
a transformed run equals the original's, over a range of trip counts, for
both transforms, including runs that throw.

The *reverse* map is one-to-many, which is the OSR hazard: a bci in the
region has `k+1` images. `LoopXform::osr_entry_pc` returns the steady-state
one — copy `k` for peel, copy `0` for unroll. Entering a *peeled* copy from
OSR would re-run the peeled iterations and execute the loop `k` times too
many. This is the same class of bug as the LICM pre-header bypass (an entry
edge landing on the wrong side of duplicated code), so it is answered here
rather than left to the consumer.

## Interaction with the LICM pre-header bypass fix

Because the transform runs before loop detection, `find_bypassable_loop_headers`
re-runs on the rewritten bytecode and the pre-header guard keeps working
unchanged. Peeling additionally *removes* bypassability of the steady-state
loop: an external branch into the header now lands on the peeled copy, so the
only edges into copy `k` are the fall-through and the back edge, and a hoist
the guard previously had to drop becomes legal again. Unrolling does **not**
have that property — its copy 0 *is* the header — and the test says so
explicitly, so nobody assumes otherwise.

## Wiring (not done — out of scope for the file set this landed in)

To consume this, `compile_with_param_slots` (`jit/src/x64.rs`) would:

1. after `detect_loops` and before the LICM/BCE/SIMD analyses, pick a loop
   and call `plan_loop_peel` / `plan_loop_unroll`;
2. on `Ok`, compile `xform.code` instead of `code`, and use
   `xform.exception_ranges` for handler dispatch;
3. map every deopt/oop-map bci through `xform.bci_at(pc)` when recording
   frame state, and every OSR entry request through `xform.osr_entry_pc(bci)`;
4. on `Err`, compile the original — every refusal is safe to ignore.

Step 3 is the load-bearing one: without it the compiled method records
transformed PCs as interpreter bcis. Until the wiring exists, the transforms
are analysis-only and nothing in the emitter calls them.

The existing native-code unroller (`unroll_loops`) and this transform must
not both fire on the same loop; the natural end state is for the bytecode
transform to replace it, since its admission test is strictly stronger (the
native one checks only `code[back_edge] == 0xa7` and a body-size bound — no
reducibility test, no single-entry test, and it does not consult
`bypassable_headers` even though the LICM and FP hoists immediately below it
do).
