> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkHandles` passes in the 53/1 run. The "Required out-of-file change (not applied)" IS applied: `vm/src/vm/vm_exec.rs:23032` carries `asVarargsCollector` in the `check_override` disjunct, with this record's rationale comment verbatim.
>
> Previous location: `docs/known-issues/jdk-only/L2-methodtypeform-lambdaforms-null.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# A fabricated `MethodTypeForm` left both lazy caches null — `RJdkHandles` died on `asVarargsCollector`

**Status:** FIXED in source 2026-08-06 (lane L2, JDK-only wave 2). Not yet
verified against a binary — see *How to verify* below. One out-of-file patch is
required for the fix to take effect in real-JDK mode; see *Required
out-of-file change*.

## The failure

`regression-suite/src/RJdkHandles.java` fails in **both** `--real-jdk` and
`--jdk-only`, with a byte-identical trace; HotSpot 25 runs it to exit 0. The
identical trace is the tell that this is an ordinary Compatible-mode defect and
not a strict-mode policy drop — nothing was *refused*.

```
CK RJdkHandles invoke ok type=(int,int)int
Exception in thread "main" java/lang/NullPointerException: Cannot load from object array because "this.lambdaForms" is null
    at RJdkHandles.main(RJdkHandles.java:293)
    at RJdkHandles.adaptation(RJdkHandles.java:137)
    at java/lang/invoke/MethodHandle.asVarargsCollector(MethodHandle.java:1528)
    at java/lang/invoke/MethodHandleImpl.makeVarargsCollector(MethodHandleImpl.java:454)
    at java/lang/invoke/MethodHandleImpl$AsVarargsCollector.<init>(MethodHandleImpl.java:463)
    at java/lang/invoke/MethodHandleImpl$AsVarargsCollector.<init>(MethodHandleImpl.java:466)
    at java/lang/invoke/DelegatingMethodHandle.<init>(DelegatingMethodHandle.java:50)
    at java/lang/invoke/DelegatingMethodHandle.chooseDelegatingForm(DelegatingMethodHandle.java:112)
    at java/lang/invoke/DelegatingMethodHandle.makeReinvokerForm(DelegatingMethodHandle.java:120)
    at java/lang/invoke/DelegatingMethodHandle.makeReinvokerForm(DelegatingMethodHandle.java:136)
    at java/lang/invoke/MethodTypeForm.cachedLambdaForm(MethodTypeForm.java:129)
```

(The trace is printed outermost-first, so `MethodTypeForm.cachedLambdaForm` is
the deepest frame, not the shallowest.)

`RJdkHandles.adaptation` line 136-137:

```java
MethodHandle sumAll = lk.findStatic(RJdkHandles.class, "sumAll",
        MethodType.methodType(int.class, int[].class)).asVarargsCollector(int[].class);
check((int) sumAll.invoke(1, 2, 3, 4) == 10, "asVarargsCollector");
```

Everything before it in `adaptation()` passes — `insertArguments`,
`dropArguments`, `permuteArguments`, `asType`, `constant`, `identity`,
`filterArguments`, `guardWithTest` — because every one of those *is* registered
as a native and never enters the real `java.lang.invoke` adapter machinery.
`asVarargsCollector` is not, so it is the first line of the method that runs
real JDK bytecode over a CratonVM-fabricated `MethodHandle`.

## Root cause

This is candidate shape **(a)**: a native hands back a `MethodTypeForm` whose
fields never went through the real constructor. It is *not* a field-slot /
by-name-read bug — the slot indices in use are correct against JDK 25's
declaration order.

`native-builtins/src/lang_invoke.rs:8585` `populate_method_type_form` is called
by every `MethodType`-producing native — the four `MethodType.methodType(...)`
overloads (`lang_invoke.rs:575, 591, 607, 636`), the `asType`/`insertArguments`
retype path (`:4611`), `build_method_type_from_descriptor` (`:8516`, itself the
type-stamping path under `alloc_method_handle`), and `:9788`. It allocated a
`java/lang/invoke/MethodTypeForm` and wrote **only slots 0-3**:

```rust
let form = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodTypeForm", 7);
ctx.set_field(form, 0, Value::Int(slot_count));       // parameterSlotCount
ctx.set_field(form, 1, Value::Int(primitive_count));  // primitiveCount
ctx.set_field(form, 2, Value::Object(Some(mt)));      // erasedType
ctx.set_field(form, 3, Value::Object(Some(mt)));      // basicType
ctx.set_field(mt, 2, Value::Object(Some(form)));      // mt.form = form
```

Slots 4, 5 and 6 (`methodHandles`, `lambdaForms`, `interpretEntry`) were left
at their allocation default, null.

In real-JDK mode `alloc_concurrent_synthetic`
(`native-builtins/src/util_concurrent_ext.rs:800`) resolves the **real** class
id and allocates `num_fields.max(class_num_total_fields(cid))` slots — so this
object genuinely *is* a `java.lang.invoke.MethodTypeForm` to every type test
and every field read. It simply never ran `<init>`.

JDK 25 `java/lang/invoke/MethodTypeForm` (`lib/src.zip`, declaration order,
super is `Object`, so slot index == declaration index):

| slot | field | type |
| --- | --- | --- |
| 0 | `parameterSlotCount` | `short` |
| 1 | `primitiveCount` | `short` |
| 2 | `erasedType` | `MethodType` |
| 3 | `basicType` | `MethodType` |
| 4 | `methodHandles` | `SoftReference<MethodHandle>[]` |
| 5 | `lambdaForms` | `SoftReference<LambdaForm>[]` |
| 6 | `interpretEntry` | `SoftReference<MemberName>` |

and its two cache accessors, which are bare indexed loads with **no null
check**:

```java
public LambdaForm cachedLambdaForm(int which) {
    SoftReference<LambdaForm> entry = lambdaForms[which];   // <-- the aaload that NPE'd
    return (entry != null) ? entry.get() : null;
}
public MethodHandle cachedMethodHandle(int which) {
    SoftReference<MethodHandle> entry = methodHandles[which];
    return (entry != null) ? entry.get() : null;
}
```

The real constructor never produces the state we fabricated. Its tail:

```java
if (erasedPtypes == basicPtypes && basicReturnType == returnType) {
    this.basicType     = erasedType;          // a BASIC form
    ...
    this.lambdaForms   = new SoftReference[LF_LIMIT];   // both allocated
    this.methodHandles = new SoftReference[MH_LIMIT];
} else {
    this.basicType     = MethodType.methodType(basicReturnType, basicPtypes, true);
    ...
    this.methodHandles = null;                // null only on the NON-basic branch
    this.lambdaForms   = null;
}
```

Null caches are legal *only* for a non-basic form, and the JDK never calls the
accessors on one — every caller routes through `basicType().form()` first.
Our form writes `basicType = erasedType = mt` (slots 2 and 3 above), i.e. it
declares itself basic, and then carries the null caches of a non-basic form.
That combination cannot arise from the real bytecode, and the JDK has no null
check to catch it.

The path in this failure hits it directly
(`DelegatingMethodHandle.makeReinvokerForm`):

```java
MethodType mtype = target.type().basicType();   // = form.basicType = our own mt
...
form = mtype.form().cachedLambdaForm(whichCache);   // NPE
```

**This is not the first time.** `lang_invoke.rs:4633-4636` already records the
same root cause reached from the other accessor, and worked around it by
shimming the *caller*:

> `new MutableCallSite(MethodType)` (Groovy's `CacheableCallSite`) runs
> `CallSite.makeUninitializedCallSite`, which NPEs on CratonVM because
> `MethodTypeForm.methodHandles` (a lazy cache array) is null.

So the null-cache shape has now produced two independent NPEs in real JDK
bytecode, one per accessor, and had been patched once per victim rather than at
the source.

## Why populating the cache is necessary but not sufficient

Filling `lambdaForms` turns `cachedLambdaForm` into an ordinary cache **miss**,
which is correct — but `makeReinvokerForm` then goes on to *build* a
`LambdaForm` reinvoker for `MethodHandleImpl$AsVarargsCollector`, and that
reinvoker's whole job is to re-enter the wrapped target through
`MethodHandle.invokeBasic`. The wrapped target here is a CratonVM
`MH_KIND_STATIC` handle from `alloc_method_handle`
(`lang_invoke.rs:5635`) — a fabricated `java/lang/invoke/MethodHandle` with our
own `class`/`name`/`desc`/`kind`/`bound` fields at slots 16-20 and **no**
`form`, no `LambdaForm`, no `invokeBasic` body. So the cache fix alone only
moves the failure one or two frames deeper, on line 138's `sumAll.invoke(...)`.

The handle must therefore never reach `DelegatingMethodHandle` at all.

## What changed

Both changes are in `native-builtins/src/lang_invoke.rs`. Nothing was deleted,
and both registrations are `NativeKind::Bridge` (the ambient category of
`register_method_handle_combinator_extras_bridge`), so `--jdk-only` keeps them.

### 1. `asVarargsCollector` / `asFixedArity` intercepted — `lang_invoke.rs:4770-4813`

Two rows added to `register_method_handle_combinator_extras_bridge`, alongside
the existing `asCollector` / `asSpreader` pair:

| class | method | descriptor | kind |
| --- | --- | --- | --- |
| `java/lang/invoke/MethodHandle` | `asVarargsCollector` | `(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;` | `Bridge` |
| `java/lang/invoke/MethodHandle` | `asFixedArity` | `()Ljava/lang/invoke/MethodHandle;` | `Bridge` |

Both return the receiver unchanged. That is the whole adapter, and it is the
right shim rather than a new `MH_KIND_*`, because **CratonVM already applies
varargs-collector semantics at dispatch, not on the handle.**
`collect_trailing_varargs` (`lang_invoke.rs:7800`, called from the
`MH_KIND_STATIC` arm at `:6551` and the virtual arms at `:7196`/`:7225`)
collects the excess arguments of an `invoke` that supplies more flat values
than the target descriptor declares into a fresh array of the array-typed
parameter's component type. That trigger is arity/shape driven, so a "this
handle is a collector" marking would add nothing to it:

* Line 138, `sumAll.invoke(1, 2, 3, 4)` against `([I)I` — 4 flat args vs
  `p == 1`, array parameter at index 0, so the `arity_excess` trigger fires and
  `build_varargs_array` produces `int[]{1,2,3,4}` → `sumAll` returns 10.
  Note `static int sumAll(int[] xs)` (`RJdkHandles.java:173`) is **not**
  declared `int...`, so it carries no `ACC_VARARGS` — the `is_varargs` trigger
  would not have fired, `arity_excess` is what carries this case.
* Line 142, `spread.invoke(new int[]{5, 6})` after `.asFixedArity()` — exactly
  `p` args with an array already in the array slot, which
  `collect_trailing_varargs` returns untouched. That *is* fixed-arity
  behaviour, so the identity is correct for `asFixedArity` too.

Known deviation, deliberately not papered over and stated in the code comment:
`isVarargsCollector()` keeps answering `false` (the base-class bytecode, which
returns a constant `false` and does not NPE). Storing the marking would need a
sixth synthetic slot on every `MethodHandle` — `alloc_method_handle` allocates
exactly `MH_BOUND + 1` — and no caller in the corpus reads the flag back.
`withVarargs(boolean)` is deliberately *not* registered: its real bytecode is a
two-line delegation to `asFixedArity()` / `asVarargsCollector(...)`, so it now
works through the shims.

### 2. `populate_method_type_form` fills both caches — `lang_invoke.rs:8585-8667`

Slots 4 and 5 now get empty reference arrays, so the two JDK accessors read a
cache **miss** (null entry) instead of dereferencing null. This is the
root-cause half: it fixes the shape for every other real-JDK path that reaches
a fabricated form, and it is what the existing
`CallSite.makeUninitializedCallSite` shim was standing in for.

Sizes are `MTF_MH_CACHE_LEN = 16` and `MTF_LF_CACHE_LEN = 64`, deliberately
past JDK 25's `MH_LIMIT = 3` / `LF_LIMIT = 26`. Both fields are `private` in a
`final` class and are touched **only** by indexed load/store inside
`MethodTypeForm`'s own accessors — nothing reads `.length`, nothing iterates
them (checked against `lib/src.zip`) — so an over-long array is
indistinguishable from an exact one to every reader, while an array sized from
a frozen constant becomes an `ArrayIndexOutOfBoundsException` the day a JDK
release adds a cache index. `interpretEntry` (slot 6) is correctly left null:
`cachedInterpretEntry()` null-checks it.

The same edit also closes a pre-existing GC hole in this function: `mt` was
used to write slots 2/3 and `mt.form` *after* `alloc_concurrent_synthetic`,
which can collect and relocate it. `mt` and `form` are now pinned with
`pin_native_root` and re-read via `read_native_pin` before every use across the
allocation points, matching the pattern already used a few lines above at
`:8484`/`:8495`.

## Required out-of-file change (not applied — outside this lane's files)

Without this, change 1 is dead in real-JDK mode: `MethodHandle
.asVarargsCollector` is *concrete* bytecode, so `check_override` in
`vm/src/vm/vm_exec.rs` stays false and the registered native is never
consulted. The existing `asCollector`/`asSpreader` entry is the precedent, two
lines below the comment that introduces it.

**File:** `vm/src/vm/vm_exec.rs`, line 22228-22229.

Old:

```rust
                        || (class_name == "java/lang/invoke/MethodHandle"
                            && matches!(method_name, "asCollector" | "asSpreader"))
```

New:

```rust
                        // ASVARARGSCOLLECTOR/ASFIXEDARITY: the real bytecode wraps
                        // the receiver in `MethodHandleImpl$AsVarargsCollector`, a
                        // `DelegatingMethodHandle` whose ctor runs `makeReinvokerForm`
                        // -> `mtype.form().cachedLambdaForm(LF_DELEGATE)` and then
                        // needs a LambdaForm reinvoker to re-enter our synthetic
                        // handle via `invokeBasic` (no body in the `MH_KIND_*` shim
                        // model). regression-suite RJdkHandles died there with
                        // `Cannot load from object array because "this.lambdaForms"
                        // is null` in BOTH --real-jdk and --jdk-only. Pin the
                        // identity shims registered in
                        // `register_method_handle_combinator_extras_bridge`;
                        // CratonVM applies collector semantics at dispatch
                        // (`collect_trailing_varargs`).
                        || (class_name == "java/lang/invoke/MethodHandle"
                            && matches!(
                                method_name,
                                "asCollector"
                                    | "asSpreader"
                                    | "asVarargsCollector"
                                    | "asFixedArity"
                            ))
```

No companion entry is needed in `interpreter.rs`
`force_native_over_real_jdk_bytecode` — `asCollector`/`asSpreader` have none
either.

## How to verify, once a binary exists

```
cargo build --release -p cratonvm-cli

javac -d regression-suite/build regression-suite/src/RJdkHandles.java
java -cp regression-suite/build RJdkHandles                          # HotSpot 25 oracle
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkHandles
target/release/cratonvm --jdk-only -cp regression-suite/build RJdkHandles
```

All three must be byte-identical and exit 0. The two lines that close this are
`CK RJdkHandles adapt=14` (the `filterArguments` line at `RJdkHandles.java:158`,
which only prints if `asVarargsCollector`/`asFixedArity` returned first) and the
final `PASS RJdkHandles`.

If only the `vm_exec.rs` patch is missing, the failure is unchanged
(`"this.lambdaForms" is null`) because the native is registered but never
dispatched — that is the single observation that separates "the patch was not
applied" from "the diagnosis is wrong".

If the caches fix landed but the interception did not, the NPE moves off
`cachedLambdaForm` and into `LambdaForm`/`InvokerBytecodeGenerator` or an
`AbstractMethodError` on `invokeBasic`/`copyWith` — same line 137/138, deeper
frame. That is expected and is the reason both halves are here.

## Baselines

* `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — no re-freeze. The gate
  scores only rows present in both baseline and census; new rows pass and are
  reported.
* `native-builtins/tests/stub_ratchet.rs` — unchanged. It counts
  `SyntheticStub` only; both new rows are `Bridge`.
* `scripts/baselines/jdk-only-bridge-ratchet.json` — **must be re-frozen.**
  Both new rows shadow real bytecode and are not `ACC_NATIVE`, so
  `bridge_shadows_bytecode` (4581) and `bridge_without_acc_native` (9528) each
  rise by 2, and the gate runs with `slack: 0`. Re-freeze with:

  ```
  sh regression-suite/bridge-ratchet.sh --update-baseline \
     --note "MethodHandle.asVarargsCollector/asFixedArity: identity shims so a synthetic MH never reaches DelegatingMethodHandle's LambdaForm reinvoker (RJdkHandles)."
  ```

  This is the same bucket `asCollector`/`asSpreader` already sit in
  (`jdk-only-kind-map-25-linux.tsv:3936-3937`, `bridge`, not `acc_native`).
