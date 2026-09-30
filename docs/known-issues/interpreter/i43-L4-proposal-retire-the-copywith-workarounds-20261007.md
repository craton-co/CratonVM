# Proposal: retire the workarounds that exist only because `MethodHandle.copyWith` was missing

**Status: proposal, partly built in wave 44 (step 3, behind a switch that
is on; steps 1 and 2 recorded as blocked or unmeasured). Filed 2026-10-07 by
interpreter round i1 wave 43, lane L4.**

## Why

Wave 43 registered `MethodHandle.copyWith(MethodType, LambdaForm)` for
CratonVM's synthetic handles (`native-builtins/src/lang_invoke.rs`,
`register_t4_method_handle_invoke`, beside `rebind`): the view of the same
handle under a new type, `asType`'s pure copy. Until then every JDK
`java.lang.invoke` route that ended in `MethodHandle.viewAsType` threw
`AbstractMethodError: ... copyWith ... has no Code attribute`, and several
subsystems grew a native of their own to keep the JDK's code off that route.
Each of those is now a candidate for the JDK's own bytecode, and each
retired intercept is one fewer place where CratonVM answers for the JDK:

* `MethodHandles.arrayElementGetter` / `arrayElementSetter`
  (`register_array_element_accessor_bridges`, and the `check_override`
  allow-list rows in `vm/src/vm/vm_exec.rs` that pin them over the JDK
  bytecode: the "METHODHANDLES ARRAY ACCESSORS" comment names `viewAsType`
  -> `copyWith` as the reason);
* `ObjectStreamClass$RecordSupport.deserializationCtr`
  (`native_record_support_deserialization_ctr`, the `MH_KIND_RECORD_DESER`
  handle kind and its dispatch arm: "it bottoms out in `MethodHandle.copyWith`");
* the note in `native-builtins/src/reflect_annotations.rs` near the
  `copyWith` comment (an annotation-proxy path that avoided `asType`);
* the `ObjectMethods` `toString` native models in
  `vm/src/runtime/invokedynamic.rs` that exist because the JDK route failed
  (`bootstrap_record_object_method`'s getter-driven `toString`, whose
  comments cite `copyWith`): javac's shape stays native for speed, but the
  hand-assembled shapes (static, special, constructor getters) can be the
  JDK's (`i42-L4-objectmethods-getters-the-native-linkage-still-hands-to-the-jdk`).

AGENTS.md asks for fewer hard-coded class-name allow-lists; the
`check_override` rows above are one.

## Direction

One subsystem at a time, each behind the existing retirement mechanism
(`native-api/src/retired_shadow.rs`: a triple re-tagged so `--jdk-only`
yields to the JDK bytecode, `--compatible` unchanged), measured with the
probe that motivated the intercept:

1. `arrayElementGetter(byte[].class)` and friends: `ObjectStreamClass`'s
   record support, catalina `TestGenericPrincipal`, a probe of each
   primitive array type's getter and setter.
2. Record deserialization: `ObjectInputStream.readRecord` of records with
   primitive and reference components.
3. The `ObjectMethods` hand-assembled shapes: `L4W42ObjectMethodsStaticSpecialGetters`,
   `L4W43ObjectMethodsJdkRoute`.

Each step keeps the intercept if the JDK route meets another missing piece
(`copyWith` is one of several JDK-abstract `MethodHandle` / `BoundMethodHandle`
methods; `rebind` was the other registered one), and records what that piece
is.

## Cost and measurement

Correctness first: the probes above, both modes, against HotSpot 25. The
JDK routes allocate more than the intercepts (a `LambdaForm`-free synthetic
handle per adapter); record deserialization is the one path where that could
be measured on a real workload (Tomcat session replication).

## Progress (wave 44) — lane L4

Read first: `aecc8e2d0` (`MethodHandle.invokeBasic` dispatches CratonVM's
synthetic handles; the concatenation `StringConcatFactory` spins needs it),
landed with wave 43 after its first host chain.

* **Step 1 (`arrayElementGetter` / `arrayElementSetter`): kept.** JDK 25
  `MethodHandleImpl.makeArrayElementAccessor` does not stop at `viewAsType`:
  it then wraps the accessor with `makeIntrinsic` in a
  `MethodHandleImpl$IntrinsicMethodHandle`, a `DelegatingMethodHandle` whose
  constructor builds a reinvoker `LambdaForm` (`chooseDelegatingForm` ->
  `makeReinvokerForm`) and whose invocation runs it. That is the real-JDK
  handle shape the `MH_KIND_*` model cannot invoke, the reason `identity` is
  pinned. The missing piece is recorded on
  `register_array_element_accessor_bridges` and on the `check_override` row
  in `vm/src/vm/vm_exec.rs`. (`arrayLength` is the same accessor family.)
* **Step 2 (record deserialization): not retired, guard probe added.**
  `ObjectStreamClass$RecordSupport.deserializationCtr`'s JDK body chains
  `asType`, `dropArguments`, `foldArguments`, `insertArguments` and
  `arrayElementGetter`. The last two are pinned natives; `dropArguments` and
  `foldArguments` are `Bridge`s that no `check_override` row pins, so whether
  their JDK bodies (`BoundMethodHandle` species, the reason `insertArguments`
  is pinned) or the natives answer under `--jdk-only` has to be measured
  before the `MH_KIND_RECORD_DESER` native can go. Probe
  `tools/probes/interp/L4/L4W44RecordDeserialization.java` (9 rows: every
  primitive component type, references, nulls, nesting, an array component,
  records in an `Object[]`, an empty record, a compact constructor) is the
  guard to run with the native retired.
* **Step 3 (`ObjectMethods` hand-assembled shapes): built, behind
  `OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE`** (`vm/src/runtime/invokedynamic.rs`).
  A `--jdk-only` `toString` site whose getters include a method handle
  (`REF_invokeVirtual` / `REF_invokeStatic` / `REF_invokeSpecial`) goes to
  `bootstrap_generic`, the JDK's own `ObjectMethods.bootstrap`, instead of the
  waves 41-42 `RecordAccessorGetter` models. javac's shape and field-getter
  subsets stay native. The switch is `false` in the commit that adds it and
  `true` in the lane's last commit, so one revert restores the models. The
  evidence for the JDK route: the first wave-43 host chain matched HotSpot on
  `L4W43ObjectMethodsJdkRoute`'s `special-*`, `static-*` and `ctor-*` rows
  (direct `ObjectMethods.bootstrap` calls). The probes that covered the
  models and must still match: `L4W41ObjectMethodsAccessorToString` (its
  positive-control line changes: see below), `L4W42ObjectMethodsStaticSpecialGetters`,
  `L4W39ObjectMethodsGetters`, `L4W40OpenRecordObjectMethods`,
  `L4W43ObjectMethodsJdkRoute`, `L4W23RecordObjectMethods`. Positive control:
  `CRATONVM_DBG_INDY_ALL=1` prints
  `[indy-all] object-methods toString cp#N: accessor-method getters take the jdk route`
  and then `... jdk bootstrap (getters not modelled)` for each
  `L4W41ObjectMethodsAccessorToString` row, where the base printed
  `getter-driven (1 getters, 1 accessor method(s))`. When the host confirms,
  the `RecordAccessorGetter` models (`invokedynamic.rs`
  `execute_record_object_method`'s accessor arm and the
  `accessor_getters` field of `ResolvedCallSite::RecordObjectMethod` in
  `classloading/src/resolution.rs`) can be deleted.
  `object_methods_to_string_getter_refusal` stays: it answers only where the
  JDK throws, with HotSpot's measured texts.
* **Not reviewed this wave:** the `reflect_annotations.rs` `copyWith` note
  (an annotation-proxy path).
