# A bound method reference and a lambda that makes the same call are different dispatch doors — 2026-08-28

> **The `probes/` tree is no longer in the working tree.** `3b2901531`
> (*"major doc consistency update before the realeas"*, 2026-08-29) removed 915
> files and 126 525 lines, the whole probe corpus among them, and lane L3's
> twelve sweeps went the same way for consistency with that decision rather
> than surviving as the one exception. They are in history and restore in one
> command:
>
> ```bash
> for C in PropertiesShadowSweep TreeShadowSweep DequeListShadowSweep HashtableVectorShadowSweep ArrayListShadowSweep LinkedSequencedShadowSweep PqOptionalShadowSweep CollectionsShadowSweep LocaleDateTzShadowSweep MapViewsShadowSweep UtilTailShadowSweep MethodRefDoorProbe; do
>   git show b97c0cc8c:apps/probes/$C.java > probes/$C.java
> done
> ```
>
> `b97c0cc8c` is the last commit that carries all twelve with their final
> content; §7's reproduce block runs unchanged once they are back.


**Status: OPEN.** Found by L3 (`java.util` collections) while shrinking two
`Properties` probe rows; the defect is not in `java.util` and not in any one
lane's files. Reproducer: `probes/MethodRefDoorProbe.java`.

## The measurement

`x::m` and `() -> x.m()` are the same program written two ways. On this VM they
are not the same call. Five receivers, both spellings of one statement, HotSpot
25.0.4+9 as the oracle, IDENTICAL in compatible and `--jdk-only` mode:

```text
receiver                        it::remove   it.remove()   HotSpot
java.util.ArrayList             works        works         works
java.util.HashSet               ISE          works         works
java.util.HashMap.keySet()      ISE          works         works
java.util.Hashtable.keySet()    works        works         works
java.util.Properties.keySet()   ISE          works         works
```

`ISE` is `IllegalStateException`, the JDK's "remove() before next()". The
`next()` immediately before it succeeded in every row.

A non-iterator receiver is unaffected: `Runnable r = list::clear; r.run();`
empties the list.

## Why those three and not the other two

A bound method reference is an `invokedynamic` whose `LambdaMetafactory` call
site produces a **MethodHandle directly to the method**; a lambda body is an
ordinary synthetic method containing an ordinary `invokeinterface`. The
MethodHandle invocation arrives at
`vm/src/runtime/interpreter/invoke.rs`'s **stackless** path — the door
`docs/known-issues/jdk-only/` already records for `MethodHandle.invoke`'s
call-site descriptor — and that path does not consult the force-native gate.

So the reference runs the **real JDK bytecode** of the resolved method. For the
three failing receivers the iterator's class NAME is real
(`java.util.HashMap$KeyIterator` and friends) while the INSTANCE is minted by
this VM, with its snapshot fields past the class's declared ones
(`key_itr_base`). Real `HashIterator.remove()` therefore reads a `lastReturned`
that nothing wrote and raises `IllegalStateException`.

The two that survive do so for opposite reasons, which is what makes the
diagnosis a discriminator rather than a guess:

* `java.util.ArrayList$Itr` is on the bytecode-yield allow-list
  (`native_override.rs`), so BOTH doors run the real body and this VM's natives
  maintain the real fields — the two doors agree by construction.
* `java.util.Hashtable`'s view iterator is java.base's own `Hashtable$Enumerator`
  over the real table (`real_ht_view_enumerator`), so there is no synthetic
  instance for either door to disagree about.

## Why it is filed here rather than fixed

The fix belongs in the interpreter's MethodHandle path — make it consult
`should_force_registered_native_over_bytecode` the way the two other doors do —
and its blast radius is every `invokedynamic`-produced handle in the VM, not one
family of `java.util`. L3's own two rows are now measured with a direct call,
which is what the probe is about.

**It is almost certainly wider than iterators.** Any method served by a native
over a receiver this VM mints, reached through a bound method reference, takes
the bytecode door. `java.util` is simply where a probe happened to write
`it::remove`.

## The probe-hygiene consequence, which cost this lane a false lead

`t("tag", x::m)` and `t("tag", () -> x.m())` measure different things, and the
first is not what a probe of `x`'s family means to ask. Two `Properties` rows
read as a `Properties` defect for a full build cycle:
`MapViewsShadowSweep` exercised the same statement on the same class and PASSED,
because it wrote the direct call — and its one method-reference row
(`remove()` twice, which expects `IllegalStateException`) could not
discriminate, since the wrong door produces the right exception there by
accident.

All 59 bound method references in the L3 probes are now lambdas. A probe must
ask its family, not the door it happened to take.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out MethodRefDoorProbe
```

25 rows; 13 of them differ from HotSpot, all in the `x::m` column.
