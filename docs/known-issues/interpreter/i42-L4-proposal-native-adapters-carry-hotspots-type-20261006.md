# Proposal: every native `MethodHandle` adapter carries HotSpot's `type()`

**Status: proposal, filed 2026-10-06 by interpreter round i1 wave 42, lane
L4.** Not implemented.

## Why

The `MethodHandles` combinators CratonVM serves natively
(`native-builtins/src/lang_invoke.rs`: `filterArguments`,
`filterReturnValue`, `foldArguments`, `collectArguments`, `guardWithTest`,
`catchException`, `permuteArguments`, `insertArguments`, `dropArguments`,
...) build adapter handles whose `type` field is derived, per combinator,
from the inputs' `type` or raw `MH_DESC`, and some deliberately differ from
HotSpot's. `mhs_guard_with_test` keeps the target's RAW return type (a
`boolean` leaf behind an `asType(...Object)` stays `Z`) because the
signature-polymorphic boundary (`auto_box_return`) keys boxing off it.

So nothing can trust an adapter's `type()`:

* wave 42's `--jdk-only` argument checks of the combinators
  (`combinator_direct_type`, probe `L4W42CombinatorChecks`) compare types only
  when every input is a direct `findStatic` / `findVirtual` handle; a refused
  combination that passes through an adapter still links on CratonVM where
  HotSpot throws `IllegalArgumentException`;
* `asType` / `invokeExact` refusals (`mh_astype_refusal`) need the
  raw-descriptor escape ("our own aliasing") for the same reason;
* `MethodHandle.type()` as a program reads it (Groovy's `Selector`, SpEL's
  `FunctionReference`) has been wrong in several past waves, one combinator
  at a time.

## Direction

Separate the two things the `type` field is used for:

* `type` is always exactly HotSpot's `type()` for the adapter (computed by
  the JDK's own rule for that combinator: `filterReturnValue` changes the
  return, `foldArguments` drops the folded position, `guardWithTest` is the
  target's type, ...);
* the dispatch-side facts that today ride on `type` / `MH_DESC` (the leaf's
  real return type for boxing, the erased shapes) move to a slot of their own
  (for example `MH_LEAF_DESC`), read by `auto_box_return` and the dispatch.

Then the combinator checks can drop the direct-handle restriction, and the
raw-descriptor escapes can be deleted one by one, each with its probe.

## Cost and measurement

Link-time only (adapter construction), plus one slot per adapter. Measure:
the combinator probes of waves 23-42 (`L4W23ForcedCombinators`,
`L4W42CombinatorChecks`, extended with adapter inputs), the Groovy and JRuby
rows of the suite (the two heaviest adapter users), and a Spring Boot fat
jar under `--jdk-only`.
