# Lane — `MethodHandles` combinators: 16 of 56 wrong, mode-independent

**Status: OPEN. New lane, opened 2026-09-17** from
[`lane-backlog-baseline-and-diagnostics-20260917.md`](lane-backlog-baseline-and-diagnostics-20260917.md).
**Prefix:** `java/lang/invoke/` — one of L3's former prefixes (the other three,
`java/lang/reflect/`, `jdk/internal/reflect/`, `sun/reflect/`, went to
`lane-classloading-reflection-surface-20260917-RETIRED-20260918.md`, retired —
all four findings fixed);
L3 is `RETIRED` and the prefix is free. **Both modes fail identically** — these
are ordinary defects, not `--jdk-only` violations, so item 1 of
[`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md)
applies but §2b's ownership-table/retirement machinery does not.

## Source

[`../../internal/retired/methodhandles-combinator-surface-16-of-56-RETIRED-20260917.md`](../../internal/retired/methodhandles-combinator-surface-16-of-56-RETIRED-20260917.md)
— read it in full; §5 records that a prior fix attempt on

`claude/l5-mh-combinators-wip-20260902` **failed and was reverted**, and it
locates the cause precisely enough that the next attempt should not repeat it.

## What is wrong, by how badly

Instrument: `probes/MhCombinatorSweep.java`, 56 rows, JDK 25.0.3+9. 40 rows are
correct in both modes (the argument-surgery half:
`permuteArguments`/`insertArguments`/`dropArguments`/`filterArguments`/
`asSpreader`/`asCollector`/`guardWithTest`/`catchException`/`asType`/etc — do
not re-sweep these). Sixteen are wrong, in ascending order of how much of the
lane this order buys you cheaply first:

1. **Silent wrong answers (3 rows)** — worst class. `asSpreader` with a wrong
   array length or a null array fabricates arguments (`a|b|null`,
   `null|null|null`) where the JDK throws `IllegalArgumentException`/
   `NullPointerException`. `explicitCastArguments` on a cast-to-`byte` returns
   `0` instead of the real value. Fix these first — a caller cannot tell a
   wrong answer from a right one without an oracle.
2. **A whole JDK 9 API absent (5 rows)** — `countedLoop`, `whileLoop`,
   `doWhileLoop`, `iteratedLoop`, `tryFinally`. Four `NullPointerException`,
   one `NoSuchMethodError`. Largest bucket, purely additive (nothing currently
   passing depends on these being absent).
3. **Missing argument validation (3 rows)** — `insertArguments` over-supplied,
   `catchException` with a mismatched handler type, `constant(int.class, null)`
   all accepted where the JDK refuses. Same species as the `Lookup.define*`
   fix landed 2026-08-30: the JDK's validation lives in a bytecode wrapper a
   native replaced wholesale.
4. **A stale `type()` on an otherwise-correct dispatch (1 row + siblings)** —
   `foldPrefix` returns the right value with the wrong reported type, because
   `foldArguments` prepends the combiner's result but `MH_DESC` still carries
   the leaf's original arity. **This has already broken something downstream**:
   Spring's `SpEL` reads a handle's arity to decide whether to re-wrap
   arguments, and the stale type made it double-wrap. Every arity-changing
   combinator needs a `.type()` assertion beside its value assertion, or a
   probe checking only the value will call this class of bug green.

## Why the first attempt failed, and what NOT to repeat

Four release builds, byte-identical sweep results each time — the natives
compiled (their strings are in the binary) but the registry never received
them. **Cause: the new registrar was wired into `../../../native-builtins/src/lib.rs`
inside `register_synthetic_overrides`**, `#[cfg(feature = "synthetic-jdk")]`
gated, which runs in neither real-JDK nor compatible mode.
`--dump-native-registry` showed it in one comparison — **run that comparison
before the sweep on any new attempt**, not after:

```text
--jdk-only    MethodHandles.constant      PRESENT   register_method_handles_constant_bridge
--jdk-only    MethodHandles.zero          ABSENT    register_p65_method_handles_extra
compatible    all seven new triples       ABSENT
```

Three things are required together, not any one alone (source page §5's
table):

1. **The registrar must be reachable from a path that actually runs** — next
   to `register_p63_method_handles_lookup` in `vm_init.rs`'s real-JDK
   essentials path, not from `phases_late.rs` alone.
2. **The new combinator names must be added to `vm_exec.rs::check_override`**
   — that literal list is what pins a native ahead of JDK bytecode; without an
   entry there the bytecode runs even when the native is correctly registered.
3. **No primitive value stored into `MH_BOUND`** — it is a reference slot, and
   a raw `Value::Int(0)` written into it gets nulled by the G30 guard,
   corrupting the handle and dragging down `BoundMethodHandle`/
   `ClassSpecializer` linkage. This one was already fixed once (as part of the
   failed attempt) and is worth keeping.

**Start with `arrayLength`** (source page §"Start here next time"): one
factory, one dispatch arm, no loop semantics, fails as `AbstractMethodError` —
unambiguous JDK-bytecode-running, not "our native ran and got it wrong."
Confirm green with `--dump-native-registry`, not the sweep, before moving to
the next of the five loop combinators — the wiring bug affects all of them
identically, so the first one green is the hard part.

## Landing

Land as its own change with its own gate run — do not fold into an unrelated
`lang_invoke.rs` change the way the original attempt's context did (that
review noise is part of why a 930-line unverified diff got reverted rather
than fixed forward). Order per the source page: wrong-answers (1) first, then
the loop family (2) since it is large but purely additive, then validation
(3), then the `.type()` class (4).
