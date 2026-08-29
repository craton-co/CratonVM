# A bound method reference and a lambda that makes the same call are different dispatch doors — 2026-08-28

> **The twelve probes live at `apps/probes/`.** `3b2901531` (*"major doc
> consistency update before the realeas"*, 2026-08-29) moved `probes/` to
> `apps/probes/`, and the move deleted the files that were still only on lane
> branches -- these twelve among them. They are restored at the new path,
> alongside the rest of the corpus. `apps/` is in `.gitignore`, so anything
> added there needs `git add -f`; that is why the move carried some probe files
> across and dropped others.


**Status: FIXED 2026-08-29**, by the lane that found it. All 25 rows of
`apps/probes/MethodRefDoorProbe` are now identical to HotSpot 25.0.4+7 in BOTH
modes; before the fix, 13 of them differed in both. The mechanism, the fix and
what it deliberately does not touch are in "The mechanism, measured" and
"The fix" below. The rest of this page is as it was written, because the
probe-hygiene lesson at the end outlived the defect.

Found by L3 (`java.util` collections) while shrinking two
`Properties` probe rows; the defect is not in `java.util` and not in any one
lane's files. Reproducer: `apps/probes/MethodRefDoorProbe.java`.

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

## The mechanism, measured

The first reading of this page said the stackless MethodHandle path "does not
consult the force-native gate". That was close, and wrong in the way that
matters: **it consults the gate with the class that DECLARES the method, while
ordinary virtual dispatch consults it with the class of the RECEIVER.** For this
VM's collection iterators those are two different names.

```text
receiver's runtime class   java/util/HashMap$KeyIterator   <- the native is registered HERE
declares remove()?         no
resolves to                java/util/HashMap$HashIterator  <- and the probe asked HERE
```

Printed from the registry dump of a run that reproduces it:

```text
java/util/HashMap$KeyIterator  remove ()V  bridge  owns_slot=true  has_code=false
```

`owns_slot: true` with `has_code: false` is the tell, and it is the same shape
`a-native-for-an-interface-default-method-must-name-the-interface` records: the
registration is real, it owns its slot, and the door that would use it never
asks for it. `hasNext()` on the SAME class has the same shape and fires 681
times in a two-line probe, because a for-each loop calls it through
`invokeinterface` — the door that asks with the receiver's class.

That also re-explains the two survivors more precisely than the guess above:
`ArrayList$Itr` DECLARES its own `remove()`, so the two names coincide, and
`Hashtable`'s enumerator has no native in front of it at either door.

## The fix

`vm/src/runtime/interpreter/lambda.rs`, in the `InvokeVirtual | InvokeInterface`
arm of `try_lambda_dispatch`: probe the registry with the receiver's exact class
and dispatch that native through `safe_native_call`, ahead of the branches that
resolve first. Plus `build_lambda_impl_cached`, which cached the resolved
bytecode after the same declaring-class-only probe, now walks receiver → declaring
and declines the cache when any class on that chain carries the native.

**Scoped to a receiver class that does NOT declare the method.** Where the class
declares one, the native and the body are on the same class, resolution lands on
the name the registration is keyed to, and the existing precedence rules already
choose between them — this must not pre-empt that decision.

That scope is deliberate and it is the interesting number on this page. A
registry dump of one small probe run holds **977** registrations that own their
slot on a LOADED class which does not declare the method. Making all of them
fire — by teaching `invoke_on_class_shared_inner`'s `prefer_exact_class_native`
the same rule — is a defensible change and a much larger one: it would activate
up to 977 currently-unreachable natives at once. It is not what closing 13 probe
rows should buy, and it is written down here as the campaign it is rather than
taken as a side effect.

## What the original filing said



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
cratonvm --java-home "$JDK" --jdk-only -cp apps/probes/out MethodRefDoorProbe
```

25 rows; 13 of them differ from HotSpot, all in the `x::m` column.
