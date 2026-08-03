# COV-05 — `checkcast`/`instanceof` refuses more methods than every opcode gap combined

**Status:** not started. **Independent of every other lane.**
**Owns:** the `!scan.typecheck_ops.is_empty()` conjunct of `ir_compatible`
(`jit/src/ir.rs:6066`), the `0xc0`/`0xc1` arms it would admit, and their
lowering. **One conjunct** — `cov-06` and `cov-07` each own a different one,
and they are adjacent lines in the same function.

## The measurement

**306 events** — the largest single refusal anywhere in the survey, larger than
all 273 opcode-gap events put together. A method containing one `checkcast` is
refused whole, before `IrBuilder::build` runs, so these methods are invisible in
the opcode histogram.

Spring-shaped code is generics-heavy and generics compile to `checkcast`. Expect
this ranking to hold on any framework workload and to be much weaker on the
bench kernels — which reach the tier seven times in total, see `meas-02`.

## What the single-pass backend already has

`typecheck_info` is a caller-supplied per-pc table of
`(pc, *const u8 class-name, len)`, and `compile_with_param_slots` receives it
alongside `field_info` and the rest. The single-pass backend's `0xc0`/`0xc1`
arms are the reference behaviour, including the `instanceof` answer for a
primitive array (`reference_jit_instanceof_primitive_array_objectarray` is a
recorded past defect there — read it before reimplementing the predicate).

So this lane is not "invent a subtype test". It is "give the IR tier the test
the other backend already performs", which makes `ir_vs_singlepass` the natural
oracle for every case.

## The first increment

**`instanceof` only, and only against a class that is already loaded.** It
produces an `int`, it cannot throw, and it has no control-flow consequence — so
it is the half of the pair with no exception edge. Refuse `checkcast` and refuse
an unloaded target, exactly as today.

Then `checkcast`, which is the harder half for one reason: on failure it
**throws**, so it needs an exception edge, and `scan.has_athrow` is `cov-07`'s
conjunct. Check whether a `checkcast` failure routes through the same machinery
`athrow` does before assuming the two lanes are independent — if they are not,
say so here and sequence them rather than discovering it in a merge.

## How to verify

* `CRATONVM_DBG=ir-compiles` on `ConditionalOnPropertyTests`:
  `ir_compatible refused: !scan.typecheck_ops.is_empty()` falls,
  `admitted to the optimizing pipeline` rises by the same amount, and
  `optimizing backend produced a body` rises by **less** — the difference is
  methods that were hiding behind this conjunct and fail on an opcode gap the
  moment they are admitted. That difference is a result, not a regression, and
  it will re-rank `cov-01`/`cov-02`. Re-run the survey after this lane lands.
* `jit/tests/ir_vs_singlepass.rs`: the exact-class hit, the subclass hit, the
  interface hit, the miss, and `null` — which is `false` for `instanceof` and a
  no-op for `checkcast`, and is the case a hand-written predicate gets wrong.
* The primitive-array and `Object[]` cases named in the recorded defect above.

## What to refuse

A typecheck against a class that is not loaded at compile time. Resolution can
throw and can run `<clinit>`; a subtype test that quietly answers `false`
instead of resolving is a wrong answer, and it is the kind that surfaces far
from its cause.

Do not delete a neighbouring conjunct while you are in `ir_compatible`. Three
lanes edit that function and each owns exactly one line of it.
