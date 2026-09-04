# Making the IR tier's home slot optional

**Status: REFUTED by its own census, 2026-09-04, and superseded — see "What
the census said" below.** The safety argument in "What is already safe" is
wrong on the path that matters, and the change it proposed cannot fire on real
bytecode. The page is kept because the refutation is the useful part, and
because the census that produced it is a two-line instrument anyone can re-run.

The original framing follows unchanged; read it as the hypothesis, not as the
conclusion.

## Why

The optimizing tier is ~2.38x slower than the baseline on a field-read loop.
Measured decomposition, each arm established by removing the advantage from the
FAST tier with a same-config control (see `JIT_OPTIMIZATION.md`):

| component | factor |
|---|---|
| loop-carried values in registers (`CRATONVM_JIT_LOCAL_REGS=0`) | **1.57x** |
| 4x unrolling (`CRATONVM_DISABLE_UNROLL=1`) | ~1.12x |
| dependency latency, not volume | **1.36x** |

The last row is the one this design targets, and it is not a volume problem: at
matched settings the optimizing tier's loop body has **fewer** instructions than
the baseline's (95 against 108), fewer memory operations (32 against 41) and
fewer frame slots (15 against 24) — and is still 1.36x slower. A probe with four
independent accumulators shrinks that to 1.18x, which is the signature of a
serial dependency rather than of extra work.

The chain is visible in the emitted code: a store and a load of the same slot
two instructions apart, with the accumulator crossing the back edge through the
frame.

## The structural cause

`lower_data_node` gives every node a frame slot (`alloc_slot`) and writes it.
The register-residency file is a **read cache layered on top of that model**. So
a value pays its store whether or not it is resident, the store-then-load pair
survives residency, and no policy change on the cache can remove a store the
model emits unconditionally.

That is why residency working and the loop not improving are consistent: on the
measured method `resident=3 (gp=3)`, and `rbx`/`r12`/`r13` appear eleven times
in the loop body, while the body still performs 22 frame loads.

## The change

**A value that lives in a register for its whole range, and that nothing reads
from memory, should have no home slot and no store.**

### The coupling that sets the shape

These three cannot be done separately:

1. Dropping the home store requires the publish to be **register to register** —
   the publish's only source today is the home word the arm just wrote.
2. A register-to-register publish requires the arm to **say where its result
   is**.
3. Today **50 arms** say it only by writing memory (`self.store_rax(slot)`),
   against **1** that passes a register (`self.gp_store_value(id, slot, src)`).

A positional trick was tried instead of (3) — record the slot, source register
and buffer position of the last home store, and take the register only if
nothing has been emitted since. It produced byte-identical code: the
precondition never holds, because arms emit after storing. The branch was
withdrawn.

So the work is: **give the lowering arms a result location**, i.e. have them
return or record `(register | slot)` rather than only writing memory. 50 store
sites, 45 `alloc_slot` sites.

### What is already safe, and was verified

* **Deopt and safepoints are covered by `pinned`.** `LiveModel` pins every phi,
  everything if liveness did not converge, and **every value named by any
  safepoint's `locals` or `stack`** — with the reason stated in the source: "the
  slot must still hold the value at *any* recorded bci, which is not a property
  a register allocator can establish". A non-pinned value is therefore named by
  no deopt frame, and dropping its home cannot corrupt a resume.
* **References are already excluded** from the GP file, and that is a safepoint
  obligation rather than tuning: `OopMapEntry` names frame slots only, so a
  reference in a register is invisible to a root walk and cannot be updated on
  evacuation. Refs keep their homes unconditionally.
* **The read side is nearly all cached**: 84 reads go through `gp_load_value`,
  13 through `slot_of` directly.

### The one hazard, and how to make it fail closed

Those 13 direct `slot_of` reads are the only way a homeless value could be read
from memory. Do **not** enumerate them into a whitelist — a wrong whitelist is a
silent miscompile, which is the failure mode this whole area keeps producing.

Instead: **`slot_of` on a value whose home was dropped fails the compile.** The
method falls back to the interpreter, which is a coverage loss and never a wrong
answer, and a census counts the refusals by call site. The first run then names
exactly which of the 13 sites need converting to `gp_load_value`, and they can be
converted one at a time with the count going to zero as evidence.

## What the census said

The design rested on one claim, in "What is already safe": that `pinned` covers
every deopt-named value, so **a non-pinned value is named by no deopt frame**.
That is true where it was checked, in `regalloc.rs`.

It is false where it is used. `plan_register_residency` calls
`live.release_deopt_pins(graph)` deliberately, and pays for the release by
keeping every home the COLOURER planned — `plan_slots` keeps every pin, and
`alloc_slot_checked` reads its answer. So a value that has a register here may
very well be named by a deopt frame, and its home word is what that frame
reads. The comment beside the `SlotClass::Pinned` check says so in passing:
after the release that class covers "exactly the deopt-named values — i.e., on
a bytecode graph, nearly all of them".

Which turns the size of the opportunity into an empirical question. Counting it
took two lines of `plan_register_residency` and one `eprintln`, and on
`FieldLoop.sum` — the loop this whole section is about — it reads:

```text
[ir-ls] resident=3 (fp=0 gp=3)
[ir-ls] home: droppable=0 blocked_deopt=1 blocked_phi=2 safepoints=16
```

**Zero of three.** Two of the resident values are phis, whose homes are written
by the edge copies rather than by a definition arm, and the third is named by
one of the sixteen safepoints. There is no population for the 50-arm refactor
to act on, and building it would have produced a sixth measured zero — the one
outcome this page was written to avoid.

## What replaced it

`blocked_phi=2` is not a disappointment; it is the address of the problem. The
loop-carried values ARE the phis, and a phi's home is written by
`emit_copy_op`, which was memory to memory — `load rax, [src]` then
`store [dst], rax` — after which `emit_phi_copies` RELOADED the word it had
just written in order to publish the phi's register. Then, because a phi
appears in its header block's node list like any other value, the generic
publish site reloaded it AGAIN, once per iteration.

Two changes, both at that one site rather than at fifty:

* `gather_phi_copies` reports the source NODE beside its frame word, so an edge
  copy reads a resident source from its register and publishes its phi's
  register from RAX at the store, instead of reloading the word two
  instructions later. The ordering argument is the one the memory schedule
  already rests on: `resolve_parallel_copy` reads every source before anything
  writes it, and a register is updated at exactly the instruction that writes
  the word, so the two go stale together.
* A value already live in its register is not published again. This applies
  ONLY to the re-publish at a definition's own position — an edge copy must
  always write the phi's register, because that is the copy.

On `FieldLoop.sum` the body loses both loop-carried reloads (`mov
rbx,[rbp-78h]` and `mov r12,[rbp-80h]` at the top of every iteration, each
waiting on the back edge's store of the same word) and the method loses 12
bytes, 1030 → 1018.

**What the deopt finding leaves open.** `FrameValue::Register`,
`RegisterLong` and `RegisterRef` already exist and are tested, so deopt
metadata CAN name a register — but the IR tier's `emit_deopt_stub` passes only
`rbp` to `ir_deopt_entry` and reserves no `SavedRegisters` region, so nothing
would fill one. Dropping the home of a deopt-named value therefore needs that
region reserved and the file spilled into it first. That is a real piece of
work with a real prize behind it, and it is now a *sequenced* one rather than
an assumption.

## How to know it worked

Do not measure this on the debug binary or without a control. The apparatus that
produced every number above:

* release build (thin LTO and `CARGO_PROFILE_RELEASE_DEBUG=0` keeps the target
  at ~1.1 GB, which matters on a host at 98%);
* arms interleaved run by run, never in blocks;
* **a second arm of one configuration**, identical to the first, so the spread
  between a config and itself is the noise floor. Two earlier conclusions were
  wrong because that arm was missing.

The number to beat is the 1.36x residual. The prediction, if this design is
right, is that it closes most of it and leaves the 1.57x register-locals share
to the allocator work that is already tracked separately.
