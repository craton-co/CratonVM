> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkHandles` passes in the 53/1 run. Both things this record left outside its own files are in the tree. The "Required out-of-file change (not applied)" is applied — `is_method_handles_varhandle_factory_native_override` exists (`vm/src/runtime/interpreter/native_override.rs:716`) and `vm/src/vm/vm_exec.rs:23008` names `("coordinateTypes", "()Ljava/util/List;")` in the `check_override` disjunct. Defect **6**, filed as "NOT FIXED — outside this lane's files" (a wrong-type `VarHandle` reference return not throwing), is **also fixed**: `vm/src/vm/vm_exec.rs:1717-1760` carries `vh_strict_reference_return`, a `CRATONVM_VH_STRICT_REFERENCE_RETURN` kill switch documented as restoring "the pre-W6-1 silent wrong answer", plus the `boxed_primitive_supertypes` assignability table behind it.
>
> Previous location: `docs/known-issues/jdk-only/W6-1-varhandle-vartype-coordinatetypes.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `VarHandle` could not describe itself — and four more defects behind `varHandles()`

**Status:** fix written 2026-08-07 (lane W6-1, JDK-only wave 2 / strict-corpus
campaign). **Not verified against a binary.** One out-of-file patch is required
(`vm/src/vm/vm_exec.rs`); a second, for an independent defect this lane
diagnosed but does not own, is written out below and is a separate landing
decision.

## The brief, and where it was wrong

The lane was opened on this finding: `varType()` and `coordinateTypes()` have
no implementation anywhere in the CratonVM tree, so `varHandles()` has never
executed. The first half is true. The conclusion drawn from it — that the two
assertions on lines 244/245 are the *only* thing between the corpus and a pass
— is not, and two sub-claims in the brief are falsified outright:

* **"`String bogus = (String) vi.get(h)` raises `ClassCastException` at a
  `checkcast`."** `javap -c` on the compiled vector shows **no `checkcast`**
  at that site: for a signature-polymorphic call javac puts the cast into the
  call site's own symbolic descriptor —
  `invokevirtual VarHandle.get:(LRJdkHandles$Holder;)Ljava/lang/String;` at
  bci 568, then `astore`/`ifnonnull`. There is nothing downstream to throw.
  See *Defect 5*.
* **"the run dies at the latest by :244."** It does, but three of the checks
  *after* :244 fail for reasons that have nothing to do with `varType()`.

`varHandles()` is 23 checks and this lane found **five** distinct defects under
them. Fixing only the named one moves the failure four lines and no further.

## Why the real JDK bodies cannot run

Both methods are concrete bytecode in JDK 25:

```
public java.lang.Class<?> varType();
   0: aload_0
   1: getstatic  VarHandle$AccessMode.SET
   4: invokevirtual accessModeType:(LVarHandle$AccessMode;)LMethodType;
   ...
public java.util.List<java.lang.Class<?>> coordinateTypes();
   ...
   4: invokevirtual accessModeType:(LVarHandle$AccessMode;)LMethodType;
   8: invokevirtual MethodType.parameterList:()Ljava/util/List;
```

`accessModeType` reads `this.vform` and walks a `VarForm`/`MethodType` chain.
A CratonVM VarHandle is `alloc_concurrent_synthetic("java/lang/invoke/VarHandle", 3)`
— the **real** class, allocated `num_fields.max(class_num_total_fields)` wide,
with CratonVM's own meaning imposed on the first three slots. JDK 25's
`VarHandle` declares exactly four instance fields (`javap -p`):

| slot | real field | what CratonVM writes there |
| --- | --- | --- |
| 0 | `vform : VarForm` | `Int(classId)` (or the element char, or null) |
| 1 | `exact : boolean` | `Int(fieldIndex)` (or the view width) |
| 2 | `methodTypeTable : MethodType[]` | `Int(kind)` |
| 3 | `methodHandleTable : MethodHandle[]` | — untouched |

So slot 0 holds an `Int`, not a `VarForm`, and the real body cannot execute at
all. The metadata `varType()`/`coordinateTypes()` need — two `Class` mirrors —
is not recorded anywhere in the 3-slot layout.

## The five defects

### 1. `varType()` / `coordinateTypes()` unimplemented (the brief's defect)

Fixed by recording the two mirrors at **slots 4 and 5**, past the real class's
four fields, so unlike slots 0-2 they alias nothing. Asking
`alloc_concurrent_synthetic` for `VH_META_NUM_FIELDS = 6` grows the object from
4 slots to 6 and moves nothing.

Reads are guarded twice — `object_num_fields(vh) > VH_COORD0` **and**
`VH_VAR_TYPE` actually holding an object — so a handle from a factory that does
not stamp (the memory-segment handles in `phases_late/foreign_ffm.rs`, which
this lane does not own, and the legacy sub-`VH_NUM_FIELDS` array convention)
reads as "no metadata" and the accessor raises
`UnsupportedOperationException` naming the kind. **It does not invent a
`Class`.** There is no JDK failure mode here — every real VarHandle can answer
— so the refusal is a deviation either way; the choice is between a deviation
that is loud and one that is a fabricated type.

Coordinates are reconstructed from the kind tag, so only the leading one is
stored: instance `{receiver}`, static `{}`, array `{arrayClass, int}`,
byte-array view `{byte[], int}`.

The list is built with `List.of(Object[])` (with `Arrays.asList` as the
synthetic-JDK fallback, matching `lang_system.rs`'s `boxed_int_list`) so the
result `equals` the `List.of(...)` the vector compares it against. The stored
mirrors are the **caller's own** `Class` objects — `Integer.TYPE` from the
`getstatic`, the `ldc`'d `Holder.class` — so `varType() == int.class` is an
identity match, which a freshly resolved mirror would not guarantee.

### 2. `findVarHandle`/`findStaticVarHandle` read the mirror with the wrong API

```rust
let class_id = match args.get(1) {
    Some(Value::Object(Some(mirror))) => ctx.class_id_of_object(*mirror),
```

`class_id_of_object` returns the heap class **of the mirror object**, i.e.
`java/lang/Class` — never the represented class. `mirror_class_id`
(`lang_class.rs`, the reverse-map lookup) is the correct call.

Consequence: no field of the requested name was ever found in
`java/lang/Class`, so `.unwrap_or(0)` fired and **every instance VarHandle in
this VM pointed at the receiver's field 0**, whatever field was asked for.
That is why checks 231-243 pass — `Holder.i` *is* field 0 — and why `vl`
(`Holder.l`, slot 1) and `vs` (`Holder.s`, slot 2) were guaranteed to fail at
:248/:251 the moment :244 stopped being the first casualty.

It also means `varType()` could not have been derived from the recorded state:
the recorded state named the wrong class.

### 3. The instance index was a declaration-order position, not a heap slot

`vh_find_instance_field` returned the `enumerate()` position over
`declared_fields`, which counts **static** fields too and ignores inherited
ones. `get_field`/`set_field` take the absolute heap index, which
`FieldMetadata::slot_index` already carries. Only accidentally equal for a
class that inherits nothing and declares no static before the field —
`Holder.i/l/s` qualify, `Holder.arr` (declared after `static int stat`) does
not.

### 4. The static index was computed over ALL declared fields

`findStaticVarHandle` used `declared_fields(cid).position(|f| f.name == name)`,
but `get_static_field`/`set_static_field` index the class's **static block**
only (see `static_field_index_by_name` in `vm_exec.rs`, which counts statics
and nothing else). `Holder.stat` is declared 4th overall and 1st among
statics — so `vstat.get()` read static index 3, out of range, and fail-safed.
`:255` (`(int) vstat.get() == 5`) could never have passed.

Both factories now also raise `NoSuchFieldException`, as the JDK does, when the
field genuinely does not exist — but **only** when the holder mirror resolved
*and* the class's fields are visible. An unresolvable mirror keeps the historic
fallback and yields an **unstamped** handle, so `varType()` refuses rather than
reporting the caller's *requested* type as fact. That distinction is the whole
point: "we cannot see this class" must not be reported as "no such field", and
a requested type must not be reported as a declared one.

### 5. The array accessors did not bounds-check

`NativeContext::get_array_element` fails safe — out of range reads back
`Int(0)`, and `set_array_element` drops the write. So
`arrayElementVarHandle(int[].class).get(arr, 7)` on a 3-element array answered
`0`, indistinguishable from a real element, where the JDK raises
`ArrayIndexOutOfBoundsException`. `:269` (`array VarHandle must bounds-check`)
could never have passed. A `vh_array_bounds_check` now runs first in all nine
`VH_KIND_ARRAY` arms.

(`getAndAdd`/`getAndAddAcquire`/`getAndAddRelease` still have **no**
array-kind arm at all and treat an array handle as a static-field handle. Not
exercised by this vector; recorded, not fixed.)

### 6 (NOT FIXED — outside this lane's files). Wrong-type access does not throw

`RJdkHandles.java:274`:

```java
String bogus = (String) vi.get(h);     // vi is an int VarHandle
check(bogus == null, "unreachable");
```

`vh_get_plain_impl` returns `vh_auto_box(Int(3))` — a live `java.lang.Integer`.
The call site's descriptor is `(LRJdkHandles$Holder;)Ljava/lang/String;`;
`unbox_poly_return` matches on the return char and for `b'L' | b'['` **returns
the value untouched**. `unbox_poly_return_checked`'s strict rule is explicitly
gated on `method_name == "invokeExact"` and documents that "the `VarHandle`
accessors are likewise permissive here". There is no `checkcast` in the
bytecode. So `bogus` is a non-null `Integer` typed as `String`, `:275` runs,
`check(false, "unreachable")` throws `AssertionError`, and the vector dies.

HotSpot raises `WrongMethodTypeException` from the access-mode type check. The
patch is written out in the lane report; it belongs in `vm_exec.rs` next to
`unbox_poly_return_checked` and is an independent defect from this lane's.

## Required out-of-file change (not applied)

`varType`/`coordinateTypes` are **concrete** bytecode, so `check_override` in
`vm/src/vm/vm_exec.rs` stays false for them and the registrations are never
consulted on that route. This is the same dead-registration trap L2 recorded
for `asVarargsCollector`. The patch is in the lane report; the anchor is the
`is_method_handles_varhandle_factory_native_override` disjunct.

**The single observation that separates "the patch was not applied" from "the
diagnosis is wrong":** if the patch is missing, `:244` fails exactly as before
with the real `varType()` body's failure (a `vform`-shaped read of an `Int`),
never with this lane's `UnsupportedOperationException` message. That message
appearing at all proves the native is being dispatched.

## How to verify

```
cd regression-suite && <cratonvm> --java-home "<jdk>" --jdk-only -cp build RJdkHandles
```

`PASS RJdkHandles (51 checks)`, byte-identical to the HotSpot 25 oracle. The
staged markers, in order: `CK RJdkHandles accessChecks ok` (already reached
today), then `CK RJdkHandles varhandle i=7 l=42 s=q arr=[1, 20, 30] ai=7` —
`l=42` proves defect 2/3, `arr=[1, 20, 30]` proves defect 5's neighbours, and
reaching the line at all proves defects 1, 4 and 6.

## Baselines

* `scripts/baselines/jdk-only-bridge-ratchet.json` — ~~**must be re-frozen.**
  `varType` and `coordinateTypes` are two new `Bridge` rows that shadow real
  bytecode and are not `ACC_NATIVE`, so `bridge_shadows_bytecode` and
  `bridge_without_acc_native` each rise by 2 and the gate runs with `slack: 0`.~~
  **CORRECTED 2026-08-07 — the counters do not move at all, and no re-freeze is
  warranted.** The two rows are registered in
  `native-builtins/src/phases_late/reflect_invoke.rs::register_p59_varhandle`,
  which is reached only through `register_phase59_natives` →
  `register_synthetic_overrides` — and `vm_init` calls that only under
  `#[cfg(feature = "synthetic-jdk")]` **and** `config.use_synthetic_jdk` at
  runtime. `regression-suite/bridge-ratchet.sh` boots the VM with `--real-jdk`,
  and the frozen artefact records `"mode": "compatible"`. So the registration
  never enters the registry in the mode the ratchet measures: both counters move
  by **0**. Corroborating: `scripts/baselines/jdk-only-kind-map-25-linux.tsv`
  carries 50 `java/lang/invoke/VarHandle` rows and **no** `varType` /
  `coordinateTypes` row.

  This becomes a real +2 only if the registration is moved onto the real-JDK arm
  (`lang_invoke.rs::register_phase54_method_handle` /
  `register_p63_method_handles_lookup`), which is what a live surface for these
  two methods requires — see the comment in `native-builtins/src/lang_invoke.rs`
  beginning *"REGISTERED HERE, not in `phases_late/reflect_invoke.rs`"*.
  Re-freeze **after** that move, not before it.

  The general rule is [§7 of *Natives over real JDK
  classes*](../../architecture/natives-over-real-jdk-classes.md): before quoting
  a census number, ask which registrars it called and **which mode it was taken
  in**. The sibling `+2` in
  [`L2-methodtypeform-lambdaforms-null.md`](L2-methodtypeform-lambdaforms-null.md)
  (`asVarargsCollector` / `asFixedArity`) **does** hold — its registrar is
  reached from `register_essential_natives_with_shims`, the real-JDK arm. Do not
  "fix" that one by analogy.
* `native-builtins/tests/stub_ratchet.rs` — unchanged; both rows are `Bridge`,
  not `SyntheticStub`.
