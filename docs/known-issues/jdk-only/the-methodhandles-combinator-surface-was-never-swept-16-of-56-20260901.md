# The `MethodHandles` combinator surface was never swept — 16 of 56

**Status: MEASURED 2026-09-01, OPEN.** Worktree `h2-known-issues-206dee`,
branch `claude/jdk-only-mode-handoff-09b48c`. Instrument:
`probes/MhCombinatorSweep.java`, 56 rows, **16 differing and identical in BOTH
modes** — ordinary defects, not mode defects.

## 1. Why this surface, and why nothing had asked it

L5's family owns `java.lang.invoke`, and three probes cover parts of it:
`L8InvokeLookupSweep` the `Lookup` surface (83 rows), `L5ModuleInvokeSweep`
`invoke`/`invokeExact` (125), `InvokeCastSweep` the argument-cast contract (29).
**None of them asks whether a COMBINATOR is right.** `InvokeCastSweep` uses five
adapters, but only as guards — rows asserting that a cast check does *not* fire
on them.

The reason to look here was a mechanism, not a hunch. Those guard rows
established that this VM keeps the **leaf member's descriptor** in `MH_DESC`
while an adapter presents a different parameter list to its caller. Every
combinator is built on that, so a stale `type()`, a mis-ordered argument list,
or a dropped wrapper is the shape to expect — and that is most of what was
found.

## 2. What is wrong

| row | HotSpot 25.0.3+9 | CratonVM (both modes) |
| --- | --- | --- |
| `l.countedLoop` | `10` | **NullPointerException** |
| `l.whileLoop` | `12` | **NullPointerException** |
| `l.doWhileLoop` | `23` | **NullPointerException** |
| `l.iteratedLoop` | `abc` | **NoSuchMethodError** |
| `g.tryFinallyNormal` | `F:ok:Y:x:x` | **NullPointerException** |
| `g.tryFinallyThrows` | `IllegalStateException` | **NullPointerException** |
| `x.explicitCastReturn` | `6` | **`0`** |
| `c.asSpreaderWrongLen` | `IllegalArgumentException` | **`a\|b\|null`** |
| `c.asSpreaderNullArray` | `NullPointerException` | **`null\|null\|null`** |
| `p.foldPrefix.type` | `(String,String)String` | **`(String,String,String)String`** |
| `k.zeroInt` | `0` | **NullPointerException** |
| `k.arrayLength` | `3` | **AbstractMethodError** |
| `p.insertTooMany` | `IllegalArgumentException` | accepted |
| `g.catchWrongType` | `IllegalArgumentException` | accepted |
| `k.constantNullPrimitive` | `NullPointerException` | accepted |

They sort into four kinds, in descending order of how badly they fail:

### (a) A WRONG ANSWER, silently — the worst three

`c.asSpreaderWrongLen` spreads a 2-element array into a 3-parameter target and
returns **`a|b|null`**. HotSpot raises `IllegalArgumentException` because the
array length is part of the spreader's contract. Nothing fails here; a caller
gets a fabricated `null` argument.

`c.asSpreaderNullArray` is the same shape with a null array: **`null|null|null`**
where the JDK throws.

`x.explicitCastReturn` — `explicitCastArguments(len, (String)byte)` on a 6-char
string answers **`0`** instead of `6`. An explicit cast that produces a zero is
indistinguishable from a correct answer at the call site.

### (b) An entire JDK 9 API absent

The whole loop family — `countedLoop`, `whileLoop`, `doWhileLoop`,
`iteratedLoop` — plus `tryFinally`. Five combinators, four of them
`NullPointerException` and one `NoSuchMethodError`. These are not edge cases:
`tryFinally` and `countedLoop` are what a bytecode generator reaches for when it
stops emitting loops by hand.

### (c) A stale `type()` on a correct dispatch

`p.foldPrefix` returns the RIGHT VALUE and reports the WRONG TYPE:
`(String,String,String)String` where the JDK says `(String,String)String`.
`foldArguments` prepends the combiner's result, so the composed handle takes one
fewer argument — and `MH_DESC` still carries the leaf's three.

This exact pairing has already cost this tree once: SpEL's `FunctionReference`
reads a handle's arity to decide whether to re-wrap its arguments, and a stale
type made it nest an extra empty `Object[]`. A probe that asserted only the
value would have called this row green, which is why every arity-changing
combinator here has a `.type()` row beside its behaviour row.

### (d) Missing argument validation

`insertArguments` with more values than parameters, `catchException` with a
handler whose leading parameter is not the caught type, and
`constant(int.class, null)` are all accepted where the JDK refuses. Same species
as the `Lookup.define*` family fixed on 2026-08-30 — the retirement surface is
the JDK's argument-validation layer, and it lives in a bytecode wrapper that a
native replaces wholesale.

## 3. What is already right, which is what makes the list specific

40 of 56 rows are 0-diff in both modes: `permuteArguments` including a
duplicated index and a bad index, `insertArguments` at a middle position,
`dropArguments` with two inserted types, `filterArguments`,
`filterReturnValue`, `collectArguments`, `asSpreader` (correct length and
partial), `asCollector`, `asVarargsCollector`/`asFixedArity`, `guardWithTest` on
both branches with its type, `catchException` handled and not-thrown,
`asType` widening, `identity`, `constant` for references and primitives,
`zero(String)`, `empty`, the array element getter/setter including
out-of-bounds and null.

So the surface is not broadly unimplemented — the argument-surgery half is in
good shape. What is missing is the **control-flow** half and the
**validation** layer.

## 4. Not yet fixed, and the reason

This was measured while a landing build was in flight for two other fixes in
the same file (`lang_invoke.rs`). Sixteen defects across five combinator
families is its own change with its own arms and gates; folding it into a build
that already carries an argument-cast fix and a `VarHandle` hot-path guard would
make the arms uninterpretable if anything moved.

The order that follows from §2: (a) first — a wrong answer is worse than a
missing one — then (b), which is the largest but is additive rather than
corrective, then (d), then (c).

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out MhCombinatorSweep
```
