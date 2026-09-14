# H19-1 — the combinators computed the right answer in the wrong class, so only a class-name screen could see them

**Status: FIXED IN SOURCE, NOT YET VERIFIED BY AN ARM.** Lane H19, 2026-08-21.

**Provenance note, because it affects how much you should trust this page.**
Lane H19 finished its source edits and died to an infrastructure fault
(`stream watchdog did not recover`) during its final read-through, before it
wrote any record. **This record was reconstructed by lane H0 from the lane's own
in-source doc comments**, which carry its measurements verbatim. Every
`MEASURED` line below is quoted from a comment the lane wrote next to the code
it changed, taken on `C:/craton/cratonvm-r5.exe` against HotSpot 25.0.3+9.
**Nothing here was re-measured by H0**, and the arm result is pending.

---

## 1. Why these were invisible

`RJdkFunctionCombinators` is one of the five standing `SUITE=all` failures, and
`H15-1` established the fact that reframes all five: **they pass under
`--jdk-only` and fail only in Compatible mode.** Strict mode drops the
`SyntheticStub` and the real JDK lambda runs. So the stand-in is the entire
defect, and **a vector turning green here is the fix, not a violation.**

MEASURED, three arms:

```
f.andThen(g).getClass()   HotSpot     Function$$Lambda/0x…
                          --jdk-only  Function$$Lambda/0x…
                          Compatible  java.util.function.Function$AndThen   <-- fabricated

f.compose(g).getClass()   HotSpot     Function$$Lambda/0x…
                          --jdk-only  Function$$Lambda/0x…
                          Compatible  java.util.function.Function$Compose   <-- fabricated

f.andThen(null)           HotSpot     NullPointerException
                          --jdk-only  NullPointerException
f.compose(null)           same shape
```

**The computed values were RIGHT in all three arms** — `andThen` 30, `compose`
21. That is the whole reason this survived: the arithmetic is correct, so
nothing that checks *the answer* can see it. Only a **class-name screen** or a
**null-contract check** catches it.

This is the same species `H15-3` named and a fourth instance of a standing
pattern in this directory: *a native that returns the right value through the
wrong object*. `H12-1` is the same shape one layer down (a bound call that gives
the right answer through the wrong door), and `H0-4` §7 is the same shape again
(a map whose `size()` and `get()` are perfect while its nodes are the wrong
class).

## 2. What changed

`compose` and `andThen` in `native-builtins/src/phases_late/streams.rs` are
guarded so real bytecode runs where the real class is available.

**`identity()` is deliberately NOT guarded here**, and the reason is recorded at
the site: `register_function_identity_natives` in `native-builtins/src/lib.rs`
registers it *again, later*, so a guard here decides nothing. It also does not
need one — MEASURED, `Function.identity()` already returns a real lambda in
Compatible mode **despite being registered**. Two registrations, one slot; only
the later one is reachable. That is `H14-1`'s duplicate-registration trap
(**162 triples registered more than once**, only the `owns_slot` one live)
showing up in the middle of a fix, and it is exactly the case where a lane
"fixes" the unreachable copy and measures no effect.

## 3. `asInterfaceInstance` — larger than the brief said

The brief described `asInterfaceInstance` as *"returns its own `MethodHandle`
argument"*. The lane found the surrounding conversion logic needed to be right
before the return could be, and built a type-conversion matrix in
`native-builtins/src/lang_invoke.rs` with MEASURED cells, including:

* **`Void` unboxes to no primitive** — every cell of its row is a refusal.
* **The whole `void` COLUMN of the return matrix is accepts**, spelled out
  explicitly rather than folded into `primitive_widens_to`, because a `V` entry
  there would wrongly claim `void` is a widening of `int` **in both
  directions**.
* The `WrongMethodTypeException` message is transcribed from a real
  `MethodType.methodType(int[].class, String[].class, Object[][].class)` run
  rather than composed by hand.

## 4. NOT VERIFIED — read this before citing the page

* **No arm has run against this change.** The lane could not build by contract
  and died before an arm was possible.
* `RJdkFunctionCombinators` is diagnosed only to its **first** failure. `H15-3`
  counted **eleven** stand-in names still registered across three registrars,
  and that count is **ARGUED from a grep, not measured**. This may be one fix or
  five. **Do not record the vector as closed on this change alone** — run it and
  report the next failure by name.
* The `identity()` reasoning in §2 rests on the duplicate registration being
  ordered as described. Confirm with `--dump-native-registry` (`owns_slot`)
  rather than by reading the two call sites.

## 5. NOMINATIONS

* **N1 — run `RJdkFunctionCombinators` and name the next failure.** One arm.
  Until then "fixed" means "its first divergence is fixed".
* **N2 — a class-name screen belongs in the corpus.** Both defects here return
  correct values through fabricated classes, and no existing vector asks
  `getClass()`. `H18`'s `OpcodeVsReflectionProbe` is the nearest thing; a
  combinator row belongs beside it.
* **N3 — `Predicate.and`/`or` and `Consumer.andThen` were already deleted**
  (H3-1's seven), and the comment at this site says they failed the same way.
  Worth confirming the deletion left the real bytecode reachable rather than
  leaving a hole — a deletion and a guard are not the same repair.
