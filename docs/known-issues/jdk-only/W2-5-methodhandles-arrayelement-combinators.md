# `MethodHandles.arrayElementGetter` returned an INERT handle — `RJdkHandles` read 0 from `a[1]`

**Status:** FIXED in source 2026-08-07 (lane W2-5, JDK-only wave 2). Not yet
verified against a binary — see *How to verify*. **No out-of-file patch is
required**: the `check_override` allow-list entry this fix depends on already
exists (it was added for the `RecordSupport` bug). One *second-order* failure
in the same method is diagnosed but deliberately **not** fixed here — see
*The next failure*.

## The failure

`regression-suite/src/RJdkHandles.java` fails in **both** `--real-jdk` and
`--jdk-only` with a byte-identical trace; HotSpot 25 runs it to exit 0. The
identical trace is the tell that this is an ordinary Compatible-mode defect,
not a strict-mode policy drop — nothing was *refused*.

```
CK RJdkHandles invoke ok type=(int,int)int
Exception in thread "main" java/lang/AssertionError: arrayElementGetter
    at RJdkHandles.main(RJdkHandles.java:293)
    at RJdkHandles.adaptation(RJdkHandles.java:147)
    at RJdkHandles.check(RJdkHandles.java:26)
```

`RJdkHandles.adaptation` lines 145-147:

```java
MethodHandle aget = MethodHandles.arrayElementGetter(int[].class);
int[] a = { 7, 8, 9 };
check((int) aget.invokeExact(a, 1) == 8, "arrayElementGetter");
```

The assertion is on the *produced value*, not on `type()` and not on a thrown
exception. The handle read `0`, not `8`.

Everything earlier in `adaptation()` passes — `insertArguments`,
`dropArguments`, `permuteArguments`, `asType`, `constant`, `identity`,
`filterArguments`, `guardWithTest`, and (since lane L2 landed)
`asVarargsCollector` / `asFixedArity`. Every one of those is a registered
native that never enters the real `java.lang.invoke` adapter machinery.

## Root cause: the handle was a placeholder, and the log said so

`MethodHandles.arrayElementGetter` / `arrayElementSetter` *were* already
pinned as natives — `register_array_element_accessor_bridges` in
`native-builtins/src/lang_invoke.rs`, plus the matching `check_override`
disjunct in `vm/src/vm/vm_exec.rs`. So the native **did** win dispatch. The
bug was in what it returned:

```rust
let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
if let Some(mt) = build_method_type_from_descriptor(ctx, "()V") { … }
Ok(Some(Value::Object(Some(obj))))
```

A bare **17-slot** `MethodHandle` with a `()V` type and no synthetic metadata
at all. CratonVM's shim model anchors its fields at `MH_BASE = 16`:
`MH_CLASS`=16, `MH_NAME`=17, `MH_DESC`=18, `MH_KIND`=19, `MH_BOUND`=20 — so on
a 17-slot object **slots 17-20 do not exist**. `alloc_method_handle`, which
every other kind uses, allocates `MH_BOUND + 1` = 21.

That was enough for the caller it was written for:
`ObjectStreamClass$RecordSupport.<clinit>` only needs a non-null entry in its
`PRIM_VALUE_EXTRACTORS` map, and the record rebuild itself is pinned separately
as `MH_KIND_RECORD_DESER`. It was never invoked — until `RJdkHandles` invoked
it.

The failing run's own log carries the proof, two lines above the
`AssertionError`:

```
gen_heap::get_field: out-of-bounds field read dropped …
  index=18 num_slots=17 class_name=java/lang/invoke/MethodHandle
gen_heap::get_field: out-of-bounds field read dropped …
  index=19 num_slots=17 class_name=java/lang/invoke/MethodHandle
```

Indices 18 and 19 are exactly `MH_DESC` and `MH_KIND` — exactly the two fields
the `MethodHandle.invokeExact` native reads, in that order, and no others
(it does not read `MH_NAME`, which is why there is no `index=17` warning).
Both reads were dropped. Then `mh_dispatch`'s `mh_read_class` found a null
`MH_CLASS` at slot 16 (in bounds, but never written) and took its
`None` fast-fail:

```rust
let class = match mh_read_class(ctx, mh) {
    Some(c) => c,
    None => return Ok(Some(Value::Object(None))),
};
```

`null` came back, the call site's descriptor is `([II)I` (verified by `javap`),
and `coerce_value_against_ret_char` turned the null into `Value::Int(0)`.
`0 != 8`. No exception anywhere — a **silent wrong answer**, which is the
worst shape this class of bug takes.

## The fix

All in `native-builtins/src/lang_invoke.rs`:

1. Two new kinds, `MH_KIND_ARRAY_GET` (24) and `MH_KIND_ARRAY_SET` (25).
2. `register_array_element_accessor_bridges` now calls a shared
   `array_element_accessor_handle`, which resolves the array `Class` mirror to
   its descriptor (`[I`), derives the JDK-contract signature —
   `([II)I` for the getter, `([III)V` for the setter — and mints a real
   handle via `alloc_method_handle` (21 slots, all metadata written, `type()`
   populated from the true descriptor).
3. A `mh_dispatch` arm for both kinds: reads/writes `array[index]`, honours a
   `bindTo`-captured array in `MH_BOUND` the way the `MH_KIND_STATIC` arm does,
   accepts a raw *or* boxed index (an `invokeWithArguments`/`Object[]`-spreading
   adapter chain hands it boxed), unboxes the stored value against a primitive
   component type, and throws a real `ArrayIndexOutOfBoundsException` on an
   out-of-range index.

Two deliberate softenings, both to avoid regressing existing callers:

* A `Class` mirror we cannot *name* degrades to `[Ljava/lang/Object;` instead
  of throwing. A hard failure here aborts `RecordSupport.<clinit>` and with it
  every record (de)serialization, surfacing as the bogus
  `no class def found: java/io/ObjectStreamClass$RecordSupport`.
* A **null** `arrayClass` likewise degrades rather than throwing NPE, because
  `vm/src/vm.rs`'s `method_handles_factories_p65` unit test calls the factory
  with `Value::Object(None)` and unwraps the result. The real JDK throws NPE;
  matching it would turn that test red for no corpus benefit.

A mirror we *can* name that is not an array type still throws
`IllegalArgumentException`, matching the JDK.

## The next failure (diagnosed, NOT fixed)

`adaptation()` has one more assertion after line 147, and it will fail:

```java
boolean threw = false;
try {
    long bogus = (long) add.invokeExact(1, 2);   // line 152
    check(bogus == 3, "unreachable");            // line 153
} catch (WrongMethodTypeException expected) { threw = true; }
check(threw, "…must throw WrongMethodTypeException");   // line 157
```

`add` is a `(II)I` handle; `javap` confirms the call site is
`MethodHandle.invokeExact:(II)J`. The JDK throws `WrongMethodTypeException`
because `invokeExact` demands an *exact* descriptor match. CratonVM will not:
the `invokeExact` native returns a boxed `Integer(3)`, and
`vm/src/vm/vm_exec.rs`'s `coerce_value_against_ret_char` sees the wrapper
class `java/lang/Integer` where ret char `J` wants `java/lang/Long`, and
**silently yields `Value::Long(0)`**. So `bogus == 0` and the run dies with
`AssertionError: unreachable` at `RJdkHandles.java:153`.

This was **not** fixed in this lane, on purpose:

* It is not reachable from `native-builtins`. A native callback receives only
  `(&mut dyn NativeContext, &[Value])`; the call-site descriptor exists only in
  the interpreter (`vm/src/runtime/interpreter/invoke.rs`, passed to
  `crate::vm::unbox_poly_return`). Any fix is a `vm/src` change.
* `register_t4_method_handle_invoke`'s `invokeExact` registration carries an
  explicit warning that a strict check here previously aborted the VM
  mid-dispatch and broke every Groovy `IndyInterface` call site
  (`WrongMethodTypeException: expected 2 args, got 1`). That warning is about
  an **arity** check; a **return-type-only** check is materially narrower
  (adapters keep their inner target's *return* type even when their apparent
  arity drifts), but it still sits on the path every signature-polymorphic
  call site in the VM funnels through — Groovy, JRuby and Panama included.

Recommended: its own lane, with `unbox_poly_return` (`vm/src/vm/vm_exec.rs`
line ~1429) as the narrowest candidate site, gated to fire only when both the
handle's `MH_DESC` return token and the call-site return char are primitives
*and differ*.

## How to verify

```
cargo build --release -p cratonvm-cli
target/release/cratonvm --real-jdk -cp regression-suite/classes RJdkHandles
target/release/cratonvm --jdk-only  -cp regression-suite/classes RJdkHandles
```

Expected after this fix: line 147 passes and the failure **moves to
`RJdkHandles.java:153` with `AssertionError: unreachable`**. That is the
success signal for this lane, not a regression. If it instead still fails at
147, or fails with `IllegalArgumentException` / `NullPointerException` out of
`arrayElementGetter`, the mirror-name resolution (`resolve_class_name_robust`
on `int[].class`) is not returning `[I` and this analysis is wrong.

The falsifying observation is narrow and cheap: run with
`CRATONVM_DBG_MH_DISPATCH=1` and look for the `[MH_DISPATCH]` line at the
`arrayElementGetter` call. It must read `class=[I name=arrayElementGetter
desc="([II)I" kind=24 argc=2`. Anything else — most tellingly `kind=1`
(the `MH_KIND_VIRTUAL` default that a dropped OOB `MH_KIND` read falls back
to) — means the new handle is not the one being dispatched.
