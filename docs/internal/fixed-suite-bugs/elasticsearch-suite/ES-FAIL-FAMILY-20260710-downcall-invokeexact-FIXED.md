# FIXED: FFM DowncallHandle invokeExact signature-polymorphic dispatch

## Status

Fixed on branch `codex/es-suite-downcall-invokeexact-20260710-025700`.

This family appeared after the StackWalker option enum and synthetic `Function.identity()` assignability fixes let Elasticsearch native-access bootstrap reach Lucene's Panama/FFM native-access path.

## Symptom

CratonVM failed at a signature-polymorphic call site for a real-JDK Panama downcall handle:

```text
java.lang.NoSuchMethodError: java/lang/foreign/DowncallHandle.invokeExact()I
```

`java/lang/foreign/DowncallHandle.invokeExact` was registered only with the generic signature-polymorphic descriptor `([Ljava/lang/Object;)Ljava/lang/Object;`. The VM fallback recognized `java/lang/invoke/*MethodHandle*` and `java/lang/invoke/*VarHandle*` receivers, but not CratonVM's synthetic `java/lang/foreign/DowncallHandle` receiver, so bytecode call sites such as `invokeExact()I` fell through to ordinary method lookup.

## Root Cause

The fix needed three pieces:

- Treat `java/lang/foreign/DowncallHandle` as a MethodHandle-style signature-polymorphic receiver in VM dispatch.
- Prefer the exact `DowncallHandle` native before the generic `java/lang/invoke/MethodHandle` fallback, because the generic fallback can consume the receiver and return primitive zero.
- Wire the active real-JDK phase-67 FFM path to the existing libffi downcall bridge. The phase-67 `Linker.downcallHandle` shim was reading instance-method arguments as if the method were static, stored function pointer `0`, created zero-field `FunctionDescriptor` objects, and did not register `DowncallHandle.invoke/invokeExact` in the active registry.

## Code Changes

- `vm/src/vm/vm_exec.rs`
  - Added helpers for MethodHandle/VarHandle signature-polymorphic receiver classification.
  - Recognizes `java/lang/foreign/DowncallHandle` as MethodHandle-compatible.
  - Tries exact DowncallHandle native descriptors before the generic MethodHandle fallback.
  - Added regression coverage for the receiver classification and exact-preference decision.

- `native-builtins/src/phases_late.rs`
  - Corrected phase-67 `Linker.downcallHandle(MemorySegment, FunctionDescriptor, Linker.Option[])` instance-argument indexing.
  - Registered `java/lang/foreign/DowncallHandle.invoke` and `invokeExact` generic signature-polymorphic descriptors to the libffi-backed downcall implementation.
  - Made phase-67 `FunctionDescriptor.of/ofVoid` retain return and parameter layouts in fields 0 and 1.

- `native-builtins/src/panama.rs`
  - Exposed `pe_downcall_invoke` inside the crate so phase-67 can reuse the existing implementation.

- `native-builtins/src/panama_libffi.rs`
  - Made layout-kind detection tolerate phase-67 size/alignment layout objects by consulting the layout object's concrete class name.

## Verification

Focused Java probe:

```java
MethodHandle mh = Linker.nativeLinker().downcallHandle(
    linker.defaultLookup().find("getpid").orElseThrow(),
    FunctionDescriptor.of(ValueLayout.JAVA_INT));
int pid = (int) mh.invokeExact();
```

Before the fix:

```text
NoSuchMethodError: java/lang/foreign/DowncallHandle.invokeExact()I
```

After the fix with `/data/data/bin/cratonvm-es-suite-downcall-invokeexact-20260710-025700-r3`:

```text
RC=0
pid=3249919
```

Rust checks:

```text
cargo test -p cratonvm-native-builtins -p cratonvm-vm downcall_handle_uses_method_handle_signature_polymorphic_dispatch -- --nocapture
```

Result: PASS.

Elasticsearch row checks with fixture `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`:

```text
others Start=1659 Count=1 UpdateMappingTests
status=FAIL rc=1 tests=5 failed=10
old DowncallHandle.invokeExact marker: none
next blocker: AbstractMethodError: java/lang/foreign/MemoryLayout.varHandle([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle; has no Code attribute

others Start=1680 Count=1 CombineIntervalsSourceProviderTests
status=CRASH rc=139 tests=0 failed=0
old DowncallHandle.invokeExact marker: none
next blocker: AbstractMethodError: java/lang/foreign/MemoryLayout.varHandle([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle; has no Code attribute
```

## Remaining Work

The next Elasticsearch-suite blocker is the separate real-JDK FFM `MemoryLayout.varHandle(PathElement...)` path. The DowncallHandle `invokeExact()I` family is fixed because the focused probe performs a real libffi downcall and the ES reruns no longer contain the old `DowncallHandle.invokeExact` marker.
