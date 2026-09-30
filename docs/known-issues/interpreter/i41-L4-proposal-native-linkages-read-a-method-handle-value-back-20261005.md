# Proposal: the native bootstrap linkages read a `MethodHandle` VALUE back to its member

**Status: proposal, filed 2026-10-05 by interpreter round i1 wave 41, lane
L4.** First stage built by wave 42 for a defect (see Progress (wave 42)).

## Why

The native linkages of `LambdaMetafactory`, `ObjectMethods` and
`SwitchBootstraps` (`vm/src/runtime/invokedynamic.rs`) read every
`MethodHandle` static argument from the constant pool
(`resolve_method_handle_full`: kind, class, name, descriptor of a
`CONSTANT_MethodHandle`). A handle that is a VALUE and not a pool entry has
no such reading, so each linkage stops at it:

* a dynamic constant that answers a `MethodHandle` at a lambda site's
  position 1 (wave 41: the checks stop undecided and `bootstrap_lambda` fails
  with its internal error `invokedynamic: cp#N is not a MethodHandle`;
  `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`, "What
  remains of row 4");
* an `ObjectMethods` getter given as a dynamic constant (the site takes the
  JDK route, whose `toString` needs `MethodHandle.copyWith`;
  `i38-L4-objectmethods-native-linkage-ignores-its-getters`);
* the classification of a condy value's class for the `ClassCastException`
  text (`LambdaTypes::method_handle_constant_class` names the class HotSpot
  resolves a CONSTANT to, by reference kind; a value has its own class).

## Direction

One reader, `method_handle_value_member(ctx, handle) -> Option<(MethodHandleKind,
class, name, descriptor)>`, in `native-builtins/src/lang_invoke.rs`, that
answers for the two shapes a direct handle has on CratonVM:

* a CratonVM shim (`MH_KIND_*`), from its `MH_CLASS` / `MH_NAME` / `MH_DESC`
  slots and its kind;
* a JDK `DirectMethodHandle` (and its `$Special`, `$Interface`,
  `$Constructor`, `$Accessor`, `$StaticAccessor` subclasses), from its
  `member` field (`MemberName`: `clazz`, `name`, `type`, `flags`, whose
  reference-kind bits give the kind), as `MethodHandleInfo` /
  `revealDirect` read it;

and `None` for anything else (a bound or adapted handle, which no linkage
could model natively anyway). The linkages then carry a `MethodHandle` the
way wave 41's `LambdaCondyArg` carries a `MethodType` descriptor: the
condy's value classified once, before the checks, and read in place of the
pool entry.

## Cost and measurement

Paid only by a site with a dynamic-constant `MethodHandle` argument, which
javac never emits: nothing on any javac site's path (the wave-41 scan of
the static-argument tags already exists). Measure only correctness: a probe
with a condy `MethodHandle` at a lambda site's position 1 (valid, `null`, a
bound handle) and as an `ObjectMethods` getter, against HotSpot 25.

## Progress (wave 42) — lane L4

Built for the defect of row 4 of
`i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked` (a condy
implementation handle failed with an internal error):
`lang_invoke::method_handle_value_member` reads the CratonVM shim of a
direct `findStatic` / `findVirtual` handle (kind 6 / 5 / 9, `MH_CLASS`,
`MH_NAME`, `MH_DESC`; the handle's `type` must be the member's own, and it
must not be bound), and the lambda linkage reads position 1 from it. Not
built: special and constructor shims (their `MH_DESC` shape was not
checked), a JDK `DirectMethodHandle`'s `member`, the `ObjectMethods` getter
and the `ClassCastException` text uses.

## Progress (wave 43) — lane L4

Built for the defect of row 4 of
`i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked` (its
Progress (wave 43)): `method_handle_value_member` also reads a
`findConstructor` shim back (`REF_newInvokeSpecial`, `<init>`, the `(..)V`
`MH_DESC`, its `type` `(..)C`), and the new
`lang_invoke::method_handle_value_not_direct` names a value that HotSpot's
`revealDirect` cannot crack (a bound or `asType`-adapted direct shim, the
combinator kinds) by its `toString()`, so a linkage can refuse it with
HotSpot's text instead of stopping undecided. Still not built: special
shims, a JDK `DirectMethodHandle`'s `member`, the `ObjectMethods` getter
read-back and the `ClassCastException` text uses.
