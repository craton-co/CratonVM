# Constructor method references (`Foo::new`) under-allocate inherited fields

## Symptom

Running JUnit5 tests via the JUnit Platform launcher, every test fails and the
GC bounds guard floods:

```
ERROR cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
  (undersized object layout — class declares more fields than the object was
   allocated with) obj=... index=0 num_slots=0 class_id=ClassId(683)
   class_name=org/junit/jupiter/engine/discovery/DefaultClassDescriptor
   real_field_count=Some(2)
```

`DefaultClassDescriptor` declares **0** own instance fields; its 2 fields are
inherited from `AbstractAnnotatedDescriptorWrapper`. JUnit's
`ClassOrderingVisitor` creates them through the constructor method reference
`DefaultClassDescriptor::new` (a `REF_newInvokeSpecial` lambda). The object was
allocated with `num_slots = 0`, so the constructor's `super(...)` `putfield`s to
the 2 inherited slots were dropped and every later read returned null.

## Root cause

The `MethodHandleKind::NewInvokeSpecial` arm of the **lambda-proxy dispatch in
`vm/src/vm/vm_exec.rs`** sized the new object with `c.fields.len()` — the
class's *own declared* fields, counting statics and **omitting inherited
instance fields**. For `DefaultClassDescriptor` that is 0 instead of the correct
`num_total_fields = 2`.

The sibling `NewInvokeSpecial` path in `vm/src/runtime/interpreter.rs`
(`dispatch_method_handle`) already used `c.num_total_fields` and carries a
comment describing this exact pitfall — the `vm_exec.rs` copy was simply missed.
The plain `new` opcode also uses `num_total_fields`. Only constructor *method
references* whose fields are (partly) inherited were affected.

## Fix

`vm/src/vm/vm_exec.rs` — size the `NewInvokeSpecial` allocation with
`c.num_total_fields` instead of `c.fields.len()`, matching the `New` opcode and
the interpreter path.

## Verification

- `DefaultClassDescriptor::new` no longer under-allocates: the
  `gen_heap::get_field/set_field … undersized object layout` errors are
  completely gone from the JUnit5 launcher run.
- Strictly more correct (under-allocation is always a bug); matches the two
  other allocation paths.

## Out of scope (separate, pre-existing — unchanged by this fix)

Once the layout is correct, the JUnit5 launcher still reports `succeeded=0` for
the sample tests: the @Test bodies never run and each test is recorded as
`NullPointerException: Cannot invoke execute on null`. That is a distinct bug in
CratonVM's JUnit Platform test-execution / node-`execute()` path, not an
allocation/layout issue.
