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

## 5. The fix attempt FAILED, and the cause is now located (2026-09-02)

Four release builds, each producing a **byte-identical** sweep. The attempt is
preserved on `claude/l5-mh-combinators-wip-20260902` (74568bbc7) and reverted
from the lane branch, because a 930-line unverified diff -- new dispatch arms
and an edit to the `check_override` pin list -- has no business riding along
with three fixes that are verified.

**The located cause: the new registrar was wired into
`native-builtins/src/lib.rs` at a line that falls inside
`register_synthetic_overrides`**, which is `#[cfg(feature = "synthetic-jdk")]`
gated and runs in NEITHER real-JDK nor compatible mode. So the natives compiled
-- their message strings are in the shipped binary -- the code was correct, and
the registry never received a single one of them.

`--dump-native-registry` said so in one comparison, and it is the measurement
the next attempt should START from rather than reach fourth:

```text
--jdk-only    MethodHandles.constant      PRESENT   register_method_handles_constant_bridge
--jdk-only    MethodHandles.zero          ABSENT    register_p65_method_handles_extra
compatible    all seven new triples       ABSENT
```

Two registrars in one file, ~600 lines apart, with different reachability.
`register_method_handles_constant_bridge` is called from the real-JDK essentials
path; `register_p65_method_handles_extra` is called only from `phases_late.rs`.
That is the same species as
`the-fix-that-changed-nothing-a-shadowed-registrar-on-the-lookup-define-doors-20260830.md`
-- a registration that exists and is never dispatched -- arriving through a
different door: not shadowed by a later twin, but never installed at all.

### The three hypotheses that were WRONG, so nobody re-tests them

Each was a real defect, each was fixed, and none of them moved a single row:

| hypothesis | what it actually was |
| --- | --- |
| the `primitive-into-reference` store in `zero` | GENUINE: `MH_BOUND` is a reference slot, and a raw `Value::Int(0)` written into it is nulled by the G30 guard. The corrupted handle then dragged in the JDK's `BoundMethodHandle`/`ClassSpecializer` machinery, which failed to link and took `zero(String)` and `empty(...)` -- two rows that had been PASSING -- down with it. Fixed. Not the cause |
| the registrar missing from the real-JDK essentials path in `vm_init.rs` | added next to `register_p63_method_handles_lookup`. Not sufficient alone |
| the seven names missing from `vm_exec.rs::check_override` | GENUINE: that literal list is what pins a native ahead of the JDK bytecode, and without an entry the bytecode runs even when the native IS registered. Necessary, not sufficient |

The last two are almost certainly still REQUIRED; they are simply not the whole
chain. A working fix needs all three at once: a registrar reachable from a path
that actually runs, the names pinned in `check_override`, and no primitive
stored into `MH_BOUND`.

### One row that was passing by accident

`k.zeroRef` -- `MethodHandles.zero(String.class).invoke()` -- was green before
any of this, and it was green for the wrong reason: the old implementation
returned an inert handle with no `MH_KIND` whose invocation produced null, and
null happens to be the right answer for a reference type. Making the handle real
is what exposed it. Worth remembering when reading the 40 passing rows in §3: a
green row on this surface is not automatically evidence that the combinator is
implemented.

### Start here next time

`arrayLength` is the cheapest row to iterate on: one factory, one dispatch arm,
no loop semantics, and it fails as `AbstractMethodError`, which is unambiguously
the JDK bytecode running rather than our native. Get that ONE row green,
confirm it with `--dump-native-registry` rather than with the sweep, and the
other five follow the same wiring.

