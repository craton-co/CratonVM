# COV-06 — `anewarray` refuses 138 methods and `newarray` is unlowered

**Status:** not started. **Independent of every other lane.**
**Owns:** the `!scan.anewarray_ops.is_empty()` and
`!scan.multianewarray_ops.is_empty()` conjuncts of `ir_compatible`
(`jit/src/ir.rs:6066`), and the `0xbc` / `0xbd` / `0xc5` arms. **Two
conjuncts** — `cov-05` and `cov-07` each own a different one in the same
function.

## The measurement

| what | events |
|---|---:|
| `ir_compatible refused: !scan.anewarray_ops.is_empty()` (`0xbd`) | 138 |
| `ir_compatible refused: !scan.multianewarray_ops.is_empty()` (`0xc5`) | 2 |
| `no lowering for opcode 0xbc` (`newarray`, primitive arrays) | 1 |

`anewarray` is a whole-method refusal, so its 138 methods never reach the
builder and are absent from the opcode histogram. `newarray` is an opcode gap,
so its methods *were* admitted and died inside — which is why the two numbers
are not comparable and why they are in one lane: they are one feature with two
different refusal mechanisms.

Read the refusal string: `(no IR lowering for 0xbd)` is written into the
`ir_compatible` conjunct itself. The conjunct exists **because** the arm is
missing, not for an independent reason. That is unusual and it is good news —
retiring the conjunct and adding the arm is one piece of work, not two.

## What already exists to build on

`IrBuilder::build` has an arm for `0xbb new` (`ir.rs:5126`), so object
allocation is modelled: there is an `Op::New`, an escape analysis that consumes
it, and a scalar-replacement path. Array allocation is the same shape with a
length operand and an element type.

The single-pass backend receives `anewarray_info` (`(pc, cp_index)`) and
`anewarray_deferred_info` for the not-yet-loaded case, exactly mirroring
`new_info` / `new_deferred_info`. The deferred split is the part to respect:
allocating an array of a class that is not loaded has to resolve first, and
resolution can throw and run `<clinit>`.

## The first increment

`newarray` (`0xbc`) — **primitive** element types only. No class resolution, no
reference element, no deferred path, and it reuses whatever `Op::New` already
does for the header and the zero-fill. One event in the survey, which is the
point: it is the increment that proves the shape with nothing else attached.

Then `anewarray` against an **already-loaded** class, refusing the deferred case
as `ir_compatible` refuses everything today. That is the 138.

`multianewarray` last, or never — 2 events, and it is a helper call in the
single-pass backend. Leaving the conjunct in place is a defensible outcome for
this lane; say so explicitly if that is the decision rather than leaving it
looking unfinished.

## How to verify

* `CRATONVM_DBG=ir-compiles`: the `anewarray` conjunct's count falls,
  `admitted` rises by the same amount, `produced a body` rises by less. As in
  `cov-05`, the shortfall is methods that were hiding behind the conjunct and
  fail on the next gap — re-run the survey afterwards and re-rank.
* `jit/tests/ir_vs_singlepass.rs`: allocate, store, read back, return; and the
  **negative-length** case, which must throw `NegativeArraySizeException` from
  both backends at the same bci.
* A moving-GC test for `anewarray`: the fresh array is a root at the next
  safepoint, and its elements are references the collector must rewrite.

## What to refuse

An array allocation whose element class is not loaded at compile time, until
the deferred path exists. And any interaction with escape analysis /
scalar replacement that this lane has not measured: an `Op::New` for an object
and an array allocation are the same shape to the *builder* and are emphatically
not the same to a scalar-replacement pass that has to model element writes.
Refuse scalar-replacing an array in this lane and say so in the code.
