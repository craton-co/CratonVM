# HIB-DEV-01 — JSON-function tests SIGSEGV: `al_state` reads ArrayList slots off a non-ArrayList receiver → garbage `Value` tag → wild jump-table read

**Severity:** High — hard native crash (`EXCEPTION_ACCESS_VIOLATION`), takes the whole VM down mid-class.
**Status:** ✅ **FIXED & VERIFIED** (fix in `../../../../native-collections/src/lib.rs`, branch `suite-dev-run`). `JsonExistsTest` now passes **3/3 (==HotSpot)**, no regressions in the equals repros.
**Mode:** Interpreter (JIT-off census). Memory-safety bug — independent of JIT.
**HotSpot (JDK 25):** all affected classes **PASS**.

## Symptom

`org.hibernate.orm.test.function.json.*` crash with `process-died rc=139` (SIGSEGV):

```
# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=... coerce_field_value_by_descriptor+0x25
# Faulting access: read at address 0x00007FF7CFAF9214   (far outside the heap)
Native frames:
  coerce_field_value_by_descriptor   (gc/src/heap.rs)
  vm_exec::get_field
  native_collections::al_state
  native_collections::native_al_equals
  ... (map↔list equals nesting) ...
```

Affected (this dev run): `JsonExistsTest`, `JsonQueryTest`, `JsonValueTest`, `JsonTableTest` (SIGSEGV); `JsonArrayUnnestTest` (timed out — same family). The crash fires during `prepareData`/persist, while Hibernate **dirty-checks** the `Map<String,Object>` JSON attribute.

## Root cause

The trigger is a value comparison `ArrayList.equals(String)` deep inside `HashMap.equals` of the nested JSON map (Hibernate compares the current JSON `Map` value against its deep-copied snapshot; after a JSON round-trip a `theArray` value is a `List` on one side and a non-`List` on the other).

`native_al_equals(this=ArrayList, other)` calls `al_state(other)` **before** its `al_eq_operand_is_list` guard. `al_state` reads the ArrayList `elementData`/`size` slots (real-JDK indices 4 / 6) off whatever receiver it is given. Its only guard was a **field-count** check (`n_fields <= data_slot || n_fields <= size_slot`). A foreign object such as a `java.lang.String` has *enough* fields to pass that check, so `al_state` read `String`'s slot 6 as if it were `ArrayList.size`.

That slot's 16 bytes do not form a valid `Value`. `get_field` → `coerce_field_value_by_descriptor` then matches on the decoded `Value`, whose **discriminant tag is garbage**. cdb pinpointed the faulting instruction as a compiler **jump-table dispatch**:

```
movsxd rax, dword ptr [r8+rax*4]   ; r8 = jump-table base, rax = index = 0x26c27100 (a HEAP POINTER, not a small tag)
add    rax, r8
jmp    rax
```

`rax` (the match index) held a heap-pointer-sized value instead of a small enum tag, so `[r8 + rax*4]` indexed the jump table ~2.6 GB out of bounds → wild read → access violation. (The field-count guard's own comment already anticipated the Charset 3-slot case; the gap was objects with *many* fields that still aren't ArrayLists.)

## Fix

Add a **layout-class guard** to `al_state`: only read the ArrayList `elementData`/`size` slots when the receiver actually has ArrayList layout (`java/util/ArrayList` or a subclass). Known non-ArrayList classes return the empty sentinel `(None, 0)`; synthetic/unknown (class-id 0, no name) stay lenient.

```rust
fn al_is_arraylist_layout(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(obj);
    match ctx.class_id_by_name("java/util/ArrayList") {
        Some(al_id) => cid == al_id || ctx.is_subclass(cid, al_id)
            || matches!(ctx.class_name_of_id(cid), None) // synthetic/unknown — lenient
            || matches!(ctx.class_name_of_id(cid).as_deref(), Some("") | Some("java/lang/Object")),
        None => true,
    }
}
// in al_state(), after the existing field-count guard:
if !al_is_arraylist_layout(ctx, this) { return (None, 0); }
```

When `al_state` returns the empty sentinel, `native_al_equals` already falls back to the correct iterator-based comparison (and `al_eq_operand_is_list` answers "not equal" for a non-List), so behaviour stays correct — it just no longer reads raw slots off a foreign object. This also hardens **every other** `al_state` caller (`size`, `forEach`, `stream`, …) against the same foreign-receiver slot decode.

## Why this is the right layer

The deepest mechanism is that decoding an arbitrary 16-byte slot as a `Value` can produce an invalid enum discriminant, which is UB to `match`. Rather than make every slot read validate tags (hot path), the fix prevents `al_state` from reading slots off objects that don't have the layout it assumes — the actual source of the bad bytes.

## Verification

```
JsonExistsTest:  @@RESULT found=3 ok=3 failed=0  (rc=0)   # was rc=139 SIGSEGV; HotSpot = ok=3
```
Standalone no-regression repros (pass, == HotSpot): `ArrayList.equals(String)` / `Map{k->List}.equals(Map{k->String})` (`jsonrepro/JR3f`), JSON serialize+deserialize+equals round-trip (`jsonrepro/JR3e`). Full json package re-verified post-fix.

## Repro

`.cratonvm-suite/repro-json.txt` (the test class) under `@common.args`; the bug needs Hibernate's dirty-check, but the unsafe slot read is generic to `ArrayList.equals(nonArrayListWith>=7fields)`.
