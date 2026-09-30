# Lambda site validation leaves the shapes it cannot decide linked

**Status: open, narrowed (rows 2, 3 and 6 fixed by wave 38, row 1 by wave
39, row 4's common shapes by wave 41, its direct-`MethodHandle` value by
wave 42, its non-direct and constructor handle values by wave 43, and its
values of other classes by wave 46, lane L4) — filed 2026-10-01 by interpreter round i1 wave 37, lane
L4.** What remains is row 4's rarer values (a special handle of another class
(wave 44 reads a private one of the lambda's own class back), a JDK
`DirectMethodHandle` value (wave 45 width-guards it and reads a JDK
`DelegatingMethodHandle` back as its target), or a value in
`altMetafactory`'s tail (next step in Progress (wave 46)); a value of a class
outside `java.lang` is refused since wave 46)
and row 5 (a same-named class of another loader in a type's hierarchy).
Residuals of the closed
`docs/internal/fixed-bugs/interpreter-L4-native-indy-linkage-skips-the-jdks-validation-FIXED-20261001.md`.
javac emits none of these shapes; each needs a hand-assembled class file.
The effect of every row is the pre-wave-37 one: the site links where HotSpot
refuses it (or refuses it with another error).

## Progress (wave 46) — lane L4

* **Row 4, a value of a class outside the named `java.lang` ones, every
  mode** (`vm/src/runtime/invokedynamic.rs`). A dynamic constant that
  answers an object of any other class (not a `MethodHandle`, not a hidden
  class) is the new `LambdaStaticArg::Foreign`; `lambda_resolve_dynamic_args`
  computes HotSpot's two `ClassCastException` messages for it (to
  `MethodType`, to `MethodHandle`) with `exceptions::hotspot_class_cast_message`
  (made `pub(crate)`, a one-word edit in a shared file), which
  names each class's module and loader, and `metafactory_static_args`
  raises the one its position needs; `altMetafactory`'s `extractArg` refuses
  it as `argument has wrong type` through the existing kind match. A class
  whose message cannot be written stays undecided, as before. Such a site
  was the uncatchable internal error of `bootstrap_lambda` in both modes.
  Probe `tools/probes/interp/L4/L4W46LambdaSiteForeignStaticArg.java` (7
  rows: `java.util`, `java.lang.Boolean`, `java.sql` of the platform loader,
  a user class of `app`, and two `altMetafactory` rows; HotSpot 25's lines
  in its header). Positive control: `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints
  `[DBG_LAMBDA] link-check get()Ljava/util/function/Supplier;: dynamic
  static argument 0 is an object of another class` for row `list-0` (the
  base prints `... is not classified`).
* **What remains, with its next step:**
  * `altMetafactory`'s tail (a dynamic constant as the flags, a count, a
    marker or a bridge): give `LambdaCondyArg` the value's `int` (an
    `Integer`'s `value` field) and a `Class` value's internal name
    (`class_id_from_mirror`), read them in `alt_metafactory_static_args`'s
    `int_at` and marker / bridge reads before the pool, and pass the same
    `condy` slice to `bootstrap_lambda`'s three pool readers (the
    `alt_flags` read, `read_marker_interfaces`, the bridge reader), which
    today read only `ConstantPoolEntry::Integer` / class entries;
  * a special handle of a superclass method, a JDK `DirectMethodHandle`
    value, row 5: unchanged from wave 45.

## Progress (wave 45) — lane L4

* **Row 4, a JDK handle value.** `lang_invoke::method_handle_value_member`
  read `MH_KIND` and `MH_BOUND` without the width guard every other reader
  of this model's slots makes, so a condy answering a real JDK handle (a
  `DirectMethodHandle`, an `IntrinsicMethodHandle`) had two slots read out
  of its bounds. It now answers `None` for a handle without the slots, and
  reads a real JDK `DelegatingMethodHandle` back as its target
  (`mh_door_handle`, wave 45's step 1 of
  `i44-L4-proposal-invoke-a-jdk-delegating-handle-through-its-target`;
  HotSpot's `DelegatingMethodHandle.internalMemberName()` is its target's,
  so `revealDirect` cracks it as the target). A JDK `DirectMethodHandle`
  value still stops undecided. No probe: a condy that answers such a handle
  needs `--add-opens` and a hand-assembled class; read from the code only.
* **What remains, with its next step:**
  * a value of a class outside `java.lang`: `exceptions.rs`'s
    `hotspot_class_cast_message` already renders HotSpot's text for any two
    classes (one joint clause for one module, two otherwise). Give
    `LambdaStaticArg` a `Copy` variant for "an object of another class" and
    carry, in `LambdaCondyArg`, the two cast messages it can meet (to
    `java.lang.invoke.MethodType` and to `java.lang.invoke.MethodHandle`),
    computed in `lambda_resolve_dynamic_args` where `shared` / `thread` are
    in hand; `metafactory_static_args` and the `altMetafactory` twin read
    the message from there instead of `java_class()`;
  * a special handle of a superclass method (links on HotSpot; `get()`
    throws `WrongMethodTypeException`), `altMetafactory`'s tail, row 5:
    unchanged from wave 44.

## Progress (wave 44) — lane L4

* **Row 4, a `findSpecial` value of a private method of the lambda's own
  class, every mode.** `lang_invoke::method_handle_value_member` reads an
  `MH_KIND_SPECIAL` handle back as `REF_invokeSpecial` when its
  `specialCaller` is its own class (no `MH_SPECIAL_CALLER`), the method is
  PRIVATE there, and its `type` is the direct one; the site then links as
  the pool's `REF_invokeSpecial` does (HotSpot's
  `AbstractValidatingLambdaMetafactory` turns it into `REF_invokeVirtual`).
  Probe `tools/probes/interp/L4/L4W44LambdaSiteSpecialHandle.java` (row
  `private`; HotSpot 25: `priv | priv`). Positive control:
  `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints `... dynamic static argument 1 is a
  MethodHandle (REF_invokeSpecial Loprivate.priv()Ljava/lang/String;)`.
* **Measured and left:** a special handle of a SUPERCLASS method
  (`findSpecial(Base, "who", ()String, Lo)`) links on HotSpot 25 and its
  `get()` throws `WrongMethodTypeException: handle's method type (Lo)String
  but found (Base)String`; CratonVM does not read that value back (unchanged).
  Still open as well: a JDK `DirectMethodHandle` value, a value of a class
  outside `java.lang`, and `altMetafactory`'s tail.

## Progress (wave 43) — lane L4

* **Row 4, a `MethodHandle` value that is not a direct `findStatic` /
  `findVirtual` handle, every mode.**
  * A handle HotSpot's `Lookup.revealDirect` cannot crack — a `bindTo` of a
    static or virtual handle, an `asType` copy whose `type` is not its
    member's own, and the combinators (`insertArguments`, `dropArguments`,
    `permuteArguments`, `filterArguments`, `foldArguments`,
    `guardWithTest`, `constant`, `identity`, `catchException`,
    `filterReturnValue`, `collectArguments`, `tryFinally`) — is refused as
    `AbstractValidatingLambdaMetafactory` refuses it:
    `LambdaConversionException: <handle> is not direct or cannot be cracked`
    (`<handle>` is `MethodHandle` + its type, `MethodHandle()String`), in a
    `BootstrapMethodError`, recorded (measured on HotSpot 25).
    `lang_invoke::method_handle_value_not_direct` classifies the value;
    `LambdaCondyArg::not_direct` carries its text; `lambda_site_verdict`
    raises it right after the static-argument casts, before the
    `--jdk-only`-only checks, in every mode (the site was the uncatchable
    internal error `cp#N is not a MethodHandle` in both).
  * A `findConstructor` value is read back as `REF_newInvokeSpecial`
    (`method_handle_value_member`: its `MH_DESC` is `(..)V`, its `type`
    `(..)C`) and links.
  * Probe `tools/probes/interp/L4/L4W43LambdaSiteNonDirectHandle.java` (rows
    `bound`, `adapted`, `inserted`, `constant`, `ctor`; HotSpot 25's lines in
    its header). Positive control: `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints
    `[DBG_LAMBDA] link-check get()Ljava/util/function/Supplier;: dynamic
    static argument 1 is a MethodHandle (not direct: MethodHandle()String)`
    for row `bound`, then `... refused
    java/lang/invoke/LambdaConversionException: MethodHandle()String is not
    direct or cannot be cracked`; for `ctor`, `... is a MethodHandle
    (REF_newInvokeSpecial java/util/ArrayList.<init>()V)`. On the base
    (`134a197a3`) no `not direct` or `REF_newInvokeSpecial` text exists, and
    every row fails with the internal error.
* **Still open in row 4:** a special handle (`findSpecial`; its `MH_DESC`
  shape was not checked), a JDK `DirectMethodHandle` without CratonVM's
  slots, a value of a class outside `java.lang`, and `altMetafactory`'s tail
  (its reads still use the pool: `int_at`, the marker class, the bridge
  descriptor would need the condy value's `Integer` / `Class` / `MethodType`
  read back, and `bootstrap_lambda` the same).
* Row 5 untouched (blocked on the loader pages it names).

## Progress (wave 42) — lane L4

* **A dynamic constant that answers the implementation handle (position 1)
  links, every mode.** `lambda_resolve_dynamic_args` now reads a
  `MethodHandle` value back to its member when it is a DIRECT `findStatic` /
  `findVirtual` handle (`cratonvm_native_builtins::lang_invoke::method_handle_value_member`:
  reference kind 6, 5, or 9 for an interface's method, as `revealDirect`
  reports it; the class, name and descriptor from the `MH_CLASS` / `MH_NAME`
  / `MH_DESC` slots; `None` for a bound handle, an `asType`-adapted copy whose
  `type` is not the member's own, or another kind), kept in the new
  `LambdaCondyArg::method_handle`. `lambda_conversion_checks` and
  `bootstrap_lambda` read position 1 through `lambda_impl_handle_arg` (the
  value, else the pool). Such a site failed with the uncatchable internal
  error `invokedynamic: cp#N is not a MethodHandle`; HotSpot links it. This
  builds the first stage of
  `i41-L4-proposal-native-linkages-read-a-method-handle-value-back` (its
  Progress section says so).
* Probe `tools/probes/interp/L4/L4W42LambdaSiteDynamicMethodHandle.java`
  (rows `static`, `virtual` with a captured receiver, `interface`; HotSpot
  25's lines in its header, also `--compatible`'s). Positive control:
  `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints `[DBG_LAMBDA] link-check
  get()Ljava/util/function/Supplier;: dynamic static argument 1 is a
  MethodHandle (REF_invokeStatic L4W42LambdaSiteDynamicMethodHandle.hello()Ljava/lang/String;)`.
  If the member part `(...)` is missing on the host, the handle a condy's
  `Lookup.findStatic` answers there is not the shim the reader models (its
  `type` field, or its slots), and the rows still fail with the internal
  error: that is what to look at first.

### What remains of row 4 (wave 42)

* A `MethodHandle` value that is not a direct `findStatic` / `findVirtual`
  handle (bound, adapted, a constructor or special handle, a JDK
  `DirectMethodHandle` without CratonVM's slots): HotSpot's
  `AbstractValidatingLambdaMetafactory` refuses a non-direct one
  (`LambdaConversionException: ... is not direct or cannot be cracked`, not
  measured) and links the other direct kinds; CratonVM still fails with the
  internal error.
* A value of a class outside `java.lang` and `altMetafactory`'s tail: as in
  wave 41 below.

## Progress (wave 41) — lane L4

* **Row 4, the common shapes, every mode** (`vm/src/runtime/invokedynamic.rs`
  `lambda_resolve_dynamic_args`, `LambdaCondyArg`). Before the checks, every
  `CONSTANT_Dynamic` static argument of a `LambdaMetafactory` site resolves
  in argument order, as `ldc` of the entry does
  (`constants::resolve_condy_constant`; its failure is the site's failure,
  recorded through `settle_native_link_failure`), and its VALUE is classified
  as the invoker's casts see it: `null` (new `LambdaStaticArg::Null`), a
  `MethodType` (its descriptor read back through the new
  `cratonvm_native_builtins::lang_invoke::method_type_object_descriptor`), a
  `MethodHandle`, or a `java.lang` constant class. The checks
  (`lambda_site_verdict`, `metafactory_static_args`,
  `alt_metafactory_static_args`, `lambda_conversion_checks`) and
  `bootstrap_lambda` read positions 0 and 2 through `lambda_method_type_arg`
  (the value, else the pool). So a valid condy `MethodType` links; `null`
  is `metafactory`'s `Objects.requireNonNull` `NullPointerException` (no
  message; `altMetafactory`'s `extractArg` too, in its order); another
  class is the cast's `ClassCastException`; all wrapped in
  `BootstrapMethodError` and recorded. A site with no dynamic constant (every
  javac site) pays one scan of its static-argument tags, no allocation.
  `--compatible` now links the valid shape and refuses the others (every
  one was an uncatchable internal error there too).
* Probe `tools/probes/interp/L4/L4W41LambdaSiteDynamicStaticArgs.java`
  (9 rows: `mt-0`, `mt-2`, `null-0..2`, `str-0..2`, `boom-0`; HotSpot 25's
  lines in its header, measured locally with and without `-Xint`). Measured
  on the way: a condy DECLARED `MethodType` that answers a `String` fails in
  the condy's own cast (`ClassCastException: Cannot cast java.lang.String to
  java.lang.invoke.MethodType`), so the probe declares that constant
  `String` to reach the invoker's cast.
* Positive control: `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints `[DBG_LAMBDA]
  link-check get()Ljava/util/function/Supplier;: dynamic static argument 0 is
  a MethodType` (row `mt-0`), then `... validated`.

### What remains of row 4 (wave 41)

* A dynamic constant that answers a `MethodHandle` at position 1: the native
  linkage needs the handle's member (kind, class, name, descriptor) back
  from the object, which a JDK handle does not carry in a form it reads; the
  checks stop undecided and `bootstrap_lambda` still fails with its internal
  error (`invokedynamic: cp#N is not a MethodHandle`).
* A value of a class outside `java.lang` where a `MethodType` or
  `MethodHandle` belongs: HotSpot's `ClassCastException` message then names
  the value's module and loader (not measured); left undecided (internal
  error).
* A dynamic constant in `altMetafactory`'s tail (the flags, a count, a
  marker, a bridge): its value is classified, but the tail's reads
  (`int_at`, the marker name, the bridge descriptor) still read the pool, so
  the checks stop undecided and the site links with the pool's reading.

## Progress (wave 40) — lane L4

Rows 4 and 5 untouched; row 4 measured and traced so the next wave can
build it.

* **Row 4, HotSpot 25 (measured locally, a scratch generator: a condy whose
  own bootstrap answers a `MethodType`, `null` or a `String`, in
  `metafactory` position 0, 1 or 2; each site run twice):** a condy that
  answers a valid `MethodType` at position 0 or 2 LINKS and runs (`linked |
  linked`). A `null` at position 0 or 1: `BootstrapMethodError: bootstrap
  method initialization exception` caused by `NullPointerException` (message
  `null`); a `String` at position 0: the same error caused by
  `ClassCastException: class java.lang.String cannot be cast to class
  java.lang.invoke.MethodType (java.lang.String and
  java.lang.invoke.MethodType are in module java.base of loader
  'bootstrap')`, at position 1 the same with `MethodHandle`. The second
  execution throws a new `BootstrapMethodError` with the same message and no
  cause (the recorded linkage error).
* **CratonVM, read from the code:** `LambdaStaticArg::of` answers
  `Undecided` for a `CONSTANT_Dynamic`, `lambda_site_verdict` stops at `a
  dynamic static argument`, and `bootstrap_lambda` then reads position 0 and
  2 with `resolve_method_type` on the constant pool, which answers `None` for
  a condy: `VmError::Internal("LambdaMetafactory: invalid SAM erased
  MethodType")`, which Java cannot catch. So the VALID shape fails too, not
  only the refusals.
* **What would fix it:** resolve each dynamic static argument first
  (`constants::resolve_condy_constant`, whose failure is the site's, recorded);
  refuse a `null` (NPE) or a value of the wrong class (the CCE text above,
  which needs the two classes' module and loader description) as HotSpot
  does; and for a `MethodType` value, hand `bootstrap_lambda` its descriptor
  (`methodtype_to_descriptor` over a `NativeContextImpl`) in place of the
  constant-pool read, which means threading an optional override through
  `bootstrap_lambda` and `lambda_site_verdict`. A `MethodHandle` value at
  position 1 needs the handle's member (kind, class, name, descriptor) back
  from the object, which the `MH_KIND_*` shims hold in `MH_CLASS` /
  `MH_NAME` / `MH_DESC` but a JDK handle does not.
* **Row 5** stays blocked on the loader pages it names.

## Progress (wave 39) — lane L4

* **Row 1 fixed, `--jdk-only`** (`vm/src/runtime/invokedynamic.rs`
  `lambda_site_refusal`, `lambda_site_load_types`). When the checks end
  undecided, every class the call site's type and the site's `MethodType`
  static arguments name that is not loaded yet is loaded through the
  caller's loader (`resolve_class_loader_aware`), in HotSpot's order, and
  the checks run once more; a class that cannot be loaded is the
  resolution's `NoClassDefFoundError`, recorded against the instruction
  (`settle_native_link_failure`). The cost is paid only by a site the checks
  could not decide, once (a linked site is cached). Probe
  `tools/probes/interp/L4/L4W39LambdaSiteLoadsItsTypes.java`
  (`not-convertible`, `missing`, `ok`). Positive control:
  `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints `[DBG_LAMBDA] link-check
  accept()Ljava/util/function/Consumer;: loaded 1 type(s), checking again`.
* **Follow-up after the host run of `059c73509`:** `not-convertible` linked
  in both modes. The load step read only the call site's type and the
  `MethodType` arguments, so it loaded `LoIface` (and `Consumer`) but not
  `LoImpl`, which only the implementation handle's descriptor names; the
  recheck stopped undecided again at `a lambda argument` and the site linked.
  HotSpot resolves the `MethodHandle` argument too (its class, then its
  member's type). `lambda_site_load_types` now also loads the
  implementation handle's class and the classes its descriptor names, in
  argument order. Expected lines: `[DBG_LAMBDA] link-check
  accept()Ljava/util/function/Consumer;: loaded N type(s), checking again`
  then `... refused java/lang/invoke/LambdaConversionException: Type mismatch
  for lambda argument 0: interface LoIface is not convertible to class
  LoImpl`.
* **Measure before trusting it on real code:** a javac site whose types were
  not loaded at link time used to stay undecided and link; it now loads them
  (HotSpot does too) and is CHECKED. A check that answers wrongly for a
  javac site (for example through row 5's loader question) now refuses a
  valid lambda. Count `refused` lines under `CRATONVM_DBG_LAMBDA_DISPATCH=1`
  over the suite, the jdk-only corpus and a Spring Boot fat jar: javac
  output must print none, and the `loaded N type(s)` lines say how many
  sites paid for loading.
* **Rows 4 and 5 untouched.** Row 4 needs HotSpot's messages for a dynamic
  constant in each of the three `metafactory` positions and in the
  `altMetafactory` tail (a `null` value, a value of the wrong class), and
  `bootstrap_lambda` cannot consume a `MethodType` that a dynamic constant
  produced, so a correct native answer also needs the native linkage to take
  resolved values. Row 5 is a loader-identity question that the open
  `i26-L5` / `i29-L5` pages (a user loader's supertypes resolved through
  another loader) make unsafe to settle from `ClassId`s today.

## Progress (wave 38) — lane L4

**Rows 2, 3 and 6 fixed** (`vm/src/runtime/invokedynamic.rs`). HotSpot 25's
lines were measured with a scratch generator and are the header of probe
`tools/probes/interp/L4/L4W38LambdaSiteRecords.java` (19 rows).

* **Row 6, every mode.** `settle_native_link_failure` publishes a refused
  site's thrown `LinkageError` (every `BootstrapMethodError` refusal) as the
  instruction's `GenericIndyLink::Failed` row, as `settle_generic_link` does
  for a generic bootstrap. Measured: HotSpot's second execution throws a NEW
  `BootstrapMethodError` with the same message and NO cause; CratonVM now
  does the same through `raise_recorded_linkage_error` (it validated again
  and threw a new error with its cause). A `NoSuchMethodError` of a missing
  implementation (`lambda_impl_resolves`, a VM-side error the interpreter
  converts) is not recorded; HotSpot also throws a new one each time (row
  `rec-nsme`). `--compatible` records the shapes it refuses (the ones the
  native linkage cannot build). With `CRATONVM_INDY_CALLSITE_CACHE=0` nothing
  is recorded, as for the generic linkage.
* **Row 3, every mode.** `LambdaTypes::method_handle_constant_class` names
  the class of the handle HotSpot resolves the constant to, by reference kind
  (measured): `DirectMethodHandle` for `invokeStatic`/`invokeVirtual` (an
  interface's static method too), `$Interface` for `invokeInterface` (a
  default method too), `$Constructor`, `$Accessor` for an instance field
  reader or writer, `$StaticAccessor` for a static one. Still undecided (the
  site then fails with the old internal error): `invokeSpecial` (HotSpot
  checks its caller and receiver first: `IllegalAccessError`, measured), a
  member or class that is not public or not found in loaded classes (its
  resolution error comes first), and a JDK class's method, which may be
  caller-sensitive (`MethodHandleImpl$WrappedMember`, measured for
  `Class.forName`).
* **Row 2, `--jdk-only`.** Measured message: `Unsupported MethodHandle kind:
  getStatic p.C.f:()String` (`MethodHandleInfo.toString()`: the field's
  declaring class, `()T` to read, `(T)void` to write), raised by the
  constructor's `default` arm before every other check. `impl_info` renders
  field handles; a field that does not resolve in loaded classes stays
  undecided.

Positive control: `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints
`[DBG_LAMBDA] link-check recorded java/lang/BootstrapMethodError for cp#N`
once per refused row, and one `link-check ...: refused` line per row, not two.

## Evidence

`vm/src/runtime/invokedynamic.rs` `lambda_site_refusal` refuses only what it
can decide from the constant pool and LOADED classes (`LambdaStop::Unknown`
ends the walk, printed under `CRATONVM_DBG_LAMBDA_DISPATCH=1` as
`link-check ...: undecided (<what>)`). Read from the code:

1. **A type named by the factory type or the three `MethodType`s that is not
   loaded yet.** HotSpot resolves every `MethodType` static argument (and the
   call site's type) before the bootstrap runs, which loads each class, so a
   missing class is a `NoClassDefFoundError` and an existing one is known.
   CratonVM loads nothing here: every check that needs the class is undecided
   (`the functional interface`, `a lambda argument`, `a type's class`, ...).
2. **A field handle as the implementation** (`REF_getField` ...). HotSpot:
   `LambdaConversionException: Unsupported MethodHandle kind: <info>`
   (message format not measured). CratonVM: undecided, links.
3. **A `MethodHandle` constant where `metafactory` expects a `MethodType`.**
   HotSpot's `ClassCastException` names the handle's implementation class
   (`java.lang.invoke.DirectMethodHandle`, `...$Special`, ... by kind; not
   measured). CratonVM: undecided, and `bootstrap_lambda` then fails with its
   internal error (`invalid SAM erased MethodType`), which Java cannot catch.
4. **A dynamic constant among the static arguments.** HotSpot runs its
   bootstrap first (it may fail, or answer `null`: `NullPointerException`
   from `extractArg`). CratonVM: undecided.
5. **A same-named class of another loader in a type's hierarchy.** Treated as
   undecided rather than trusting the `ClassId` walk (`LambdaTypes::is_assignable`).
6. **A refused site is not recorded.** JVMS §5.4.3 has every later execution
   of a failed `invokedynamic` rethrow the SAME error; the generic linkage
   records it (`record_linkage_error`, wave 32). A refused lambda site
   caches nothing, so each execution validates again and throws a new
   `BootstrapMethodError` (observable only as `e1 != e2`).

## What would fix it

* Row 1: done (wave 39), for an undecided site only; see its Progress.
* Row 3's undecided kinds: resolve the constant (as `ldc` of it does) and
  read the class of the object it resolves to, which also raises its
  resolution error first.
* Row 4: resolve the dynamic constant (`constants::resolve_condy_constant`)
  and classify its value.
* Rows 2 (done), 3 (done for the decidable kinds), 6 (done): wave 38.
