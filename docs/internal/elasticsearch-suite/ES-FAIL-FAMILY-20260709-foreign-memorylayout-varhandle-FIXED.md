# ES failure family - MemoryLayout.varHandle has no code

Status: fixed (branch `fix/es-foreign-memorylayout-varhandle`)

Date observed: 2026-07-09
Date fixed: 2026-07-09

## Summary

Elasticsearch bootstrap paths were failing because
`java/lang/foreign/MemoryLayout.varHandle([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;` has no code attribute at call sites that invoke native-memory layout access via Panama.

Observed signatures:

```text
java.lang.AbstractMethodError: method java/lang/foreign/MemoryLayout.varHandle([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle; has no Code attribute
```

## Root cause

`java/lang/foreign/MemoryLayout` is present in CratonVM’s synthetic Panama model, but this method had no registered native implementation and was not listed in the force-native shim path. On bootstrap (`JdkZstdLibrary` and related native-access classes), the call resolved into the class with no executable code and raised `AbstractMethodError`, preventing suite setup.

## Fix

### `native-builtins/src/panama.rs`

- `register_pe2_struct_layouts`: added native registration for
  `MemoryLayout.varHandle([PathElement])VarHandle`.
- Added path-aware width derivation for `varHandle` by reusing the synthetic layout graph:
  - follows `PathElement.groupElement(name)` through `StructLayout`/`UnionLayout` members,
  - follows `PathElement.sequenceElement()` through `SequenceLayout` element layout,
  - computes the `VarHandle` access width from the selected leaf layout.
- Implemented synthetic `VarHandle` creation consistent with existing segment-kind `VarHandle` layout (`VH_CLASS_OR_TARGET`, `VH_FIELD_INDEX`, `VH_IS_STATIC`) so downstream segment access dispatch in `phases_late.rs` can execute.

### `vm/src/runtime/interpreter.rs`

- Added `MemoryLayout.varHandle([PathElement])VarHandle` to `force_native_over_real_jdk_bytecode(...)` so the registered shim is dispatched when bytecode-only declarations are encountered.

## Verification notes

- The two targeted missing-code-attribute crash families documented in this note are resolved by the shim registration and the runtime dispatch hook.
- A full Elasticsearch rerun was not executed in this change sequence; recommended follow-up is to rerun the `20260709` non-passed probes that currently surface this signal to confirm that `MemoryLayout.varHandle` no longer appears and no longer dominates the crash signal.

## Related notes

- `docs/known-issues/elasticsearch-suite/ES-FAIL-FAMILY-20260709-fail-row-summary.md`
- `docs/known-issues/elasticsearch-suite/ES-CRASH-FAMILY-20260709-currentdev-fail-probe-rc139.md`