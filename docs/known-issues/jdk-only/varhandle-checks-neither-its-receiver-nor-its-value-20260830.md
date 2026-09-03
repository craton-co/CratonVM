# `VarHandle` checks neither its receiver nor its value

**Status: MEASURED 2026-08-30, OPEN.** Worktree `h2-known-issues-206dee`,
branch `claude/jdk-only-mode-handoff-09b48c`. Instrument:
`probes/ReflectArgTypeSweep.java`, 63 rows, **both modes identical** — an
ordinary defect, not a mode defect.

## 1. What it does

`MethodHandles.lookup().findVarHandle(Box.class, "s", String.class)` produces a
handle whose every access ignores both the type of the receiver and the type of
the value.

```text
row                  HotSpot 25.0.3+9                       CratonVM (both modes)
v.wrongRef           ClassCastException                     3        <- Integer STORED in a String field
v.wrongReceiver      ClassCastException                     no-throw <- wrote through a String receiver
v.primWrongRef       WrongMethodTypeException               no-throw <- "nine" STORED in an int field
v.getWrongReceiver   ClassCastException                     y        <- READ through a String receiver
v.casWrongRef        ClassCastException                     true     <- CAS succeeded
```

Two of these are worse than a wrong exception type.

**`v.wrongRef` leaves a typed field holding the wrong type.** After
`vs.set(box, Integer.valueOf(3))` the `String`-declared field `Box.s` holds an
`Integer`. Nothing fails at the store. The next ordinary `box.s` read is where
it surfaces — as a `ClassCastException` at a site that did nothing wrong, or, if
the JIT has trusted the declared field type, as something less legible than
that.

**`v.getWrongReceiver` reads through a receiver of the wrong class and returns a
value.** `vs.get("not-a-box")` answered `y`. The field index was computed for
`Box` and applied to a `String`.

## 2. The mechanism, and why it is one line per entry point

`native-builtins/src/lang_invoke.rs`, `varhandle_set`'s `VH_KIND_INSTANCE` arm:

```rust
let receiver = match args.get(1) {
    Some(Value::Object(Some(r))) => *r,
    _ => return Ok(None),
};
let value = args.get(2).cloned().unwrap_or(Value::Int(0));
if field_idx >= 0 {
    ctx.set_field(receiver, field_idx as usize, value);
}
```

`field_idx` is resolved from the VarHandle's OWN class, then applied to whatever
object arrives. There is no arm between those two lines.

The same shape repeats across six natives — `varhandle_get`, `varhandle_set`,
`varhandle_compare_and_set`, `varhandle_compare_and_exchange`,
`varhandle_get_and_set`, `varhandle_get_and_add`, `varhandle_get_and_bitwise` —
each with its own `VH_KIND_INSTANCE` arm.

## 3. Why the fix is a fast-path design and not six `if`s

These are the CAS-dominated paths the file has already been tuned for twice: a
thread-local plan memo took the global lock off the JIT's fast paths and moved a
scaling probe from 0.07x to 0.68x at 24 threads, and `vh_meta_get` is
`Arc`-returning specifically so a hot op pays a refcount bump rather than three
`String` clones. A per-operation `class_id_by_name` + `is_subclass` would take a
class-manager read lock on every `CompletableFuture` composition step and undo
that.

`VarHandleMeta` already carries `class_id` and `field_desc`, so the check has a
cheap correct form:

* **receiver**: `ctx.class_id_of_object(recv) == meta.class_id` is an integer
  compare against a value already in hand. Only a MISMATCH — a subclass
  receiver, or a genuinely wrong one — pays the full predicate.
* **value**: `meta.field_desc` names the declared type. A reference field goes
  through `lang_invoke::reference_arg_admitted` (the shared predicate
  `MethodHandle.invoke` and `bindTo` now use, whose whole design is to refuse
  only on a positive reading); a primitive field with an object value is a
  `WrongMethodTypeException` and needs no hierarchy at all.

It should be priced with its own kill switch before it is profiled, and the
inert measurement is the one that decides the default.

## 4. The one non-`VarHandle` row in the same sweep

```text
m.interfaceWrong   Method.invoke(box, Integer)  where the parameter is an INTERFACE
  HotSpot   IllegalArgumentException
  CratonVM  InvocationTargetException   <- the callee ran and failed
```

**FIXED 2026-08-30, same day.** `Method.invoke` already checked a class-typed
parameter (`m.wrongRef` was 0-diff) and not an interface-typed one -- the same
interface-shaped hole `InvokeCastSweep`'s `x.interfaceArgWrong` recorded on the
`MethodHandle` side. Both are closed by `NativeContext::class_assignable_to_name`,
which exposes the loader-blind by-name walk `typecheck::aastore_element_assignable`
already used. See section 5 of
`the-cast-that-asType-performs-and-this-vm-did-not-20260830.md`, including the
FFM regression the first attempt caused: a check that judges interfaces MUST ask
`synthetic_implements_declared`, because a VM-minted carrier declares none.

## 5. What the sweep says is already right

53 of 63 rows are 0-diff in both modes, and the passing rows are the ones that
make the failing rows worth acting on rather than a general absence of checking:

* `Method.invoke` — wrong class argument, wrong receiver, null receiver, arity
  in both directions, primitive widening/narrowing/null/wrong-reference,
  static-ignores-receiver: all correct.
* `Field.set`/`get` — wrong reference, wrong receiver, null receiver, primitive
  widening/narrowing/null, `final` without `setAccessible`, `getInt` on a
  reference field: all correct.
* `Constructor.newInstance` — wrong reference, arity, primitive null: correct.
* `Array.set` — wrong reference, interface component right AND wrong, primitive
  wrong reference, primitive null, not-an-array, out-of-bounds: all correct.
  This is the door that already went through the shared-rule treatment.

So the defect is `VarHandle`, specifically, and the reason is visible in the
list: `Array.set` was made to ask the same question as the `aastore` bytecode,
and `VarHandle` was never asked to ask anything.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out ReflectArgTypeSweep
```
