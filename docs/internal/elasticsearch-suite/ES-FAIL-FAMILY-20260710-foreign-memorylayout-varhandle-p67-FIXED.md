# ES failure family - p67 MemoryLayout.varHandle and VarHandle access-mode metadata fixed

## Status

Fixed on branch `codex/es-suite-memorylayout-varhandle-20260710-032800`.

This is a follow-up to the earlier synthetic-FFM `MemoryLayout.varHandle` fix. The earlier fix covered the legacy synthetic layout path, but the Java 25 real-JDK/p67 registration path used by the Elasticsearch fixture still did not register `MemoryLayout.varHandle(PathElement...)`, and the VarHandle returned from the p67 shim did not expose the metadata needed by JDK `MethodHandles.insertCoordinates`.

## Symptom

After the DowncallHandle `invokeExact` fix landed, Elasticsearch rows progressed to:

```text
java.lang.AbstractMethodError: method java/lang/foreign/MemoryLayout.varHandle([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle; has no Code attribute
```

After registering p67 `MemoryLayout.varHandle`, the same rows progressed further to:

```text
java.lang.AbstractMethodError: method java/lang/invoke/VarHandle.accessModeTypeUncached(Ljava/lang/invoke/VarHandle$AccessType;)Ljava/lang/invoke/MethodType; has no Code attribute
```

After adding `accessModeTypeUncached`, the p67 VarHandle metadata initially fell back to `[Object]` coordinates, causing:

```text
java.lang.IllegalArgumentException: Invalid position 1 for coordinate types: [class java.lang.Object]
```

## Root Cause

`register_p67_foreign_memory` built p67 `StructLayout` objects and other FFM helpers, but did not register the active real-JDK `MemoryLayout.varHandle(PathElement...)` interface method.

The p67 memory-segment VarHandle also used a compact synthetic field convention, while the active receiver class is `java/lang/invoke/VarHandle` from the real JDK image. Real-JDK descriptor-aware field storage can hide those compact synthetic slots from later `VarHandle.accessModeType(...)` bytecode, so metadata could not reliably be recovered from object fields.

## Fix

- Added p67 `MemoryLayout.varHandle(PathElement...)` registration.
- Resolved named group path elements against p67 `StructLayout` members so field width is selected from the target member layout.
- Added `VarHandle.accessModeTypeUncached(VarHandle.AccessType)` native metadata support.
- Added a p67 memory-segment VarHandle identity-hash side table that records width at VarHandle creation time and is used by access-mode metadata and p67 memory-segment get/set dispatch.
- Expanded `force_native_over_real_jdk_bytecode` coverage for active p67 `MemoryLayout` construction and `varHandle` methods.

## Verification

Targeted Rust tests:

```text
cargo test -p cratonvm-native-builtins p67_memory_segment_varhandle_access_mode_type_uses_segment_and_offset_coordinates -- --nocapture
cargo test -p cratonvm-native-builtins memory_layout_varhandle_resolves_named_struct_member_width -- --nocapture
cargo test -p cratonvm-vm ffm_memory_layout_force_native_covers_varhandle -- --nocapture
```

All passed.

Elasticsearch fixture, r6 binary:

```text
/data/data/bin/cratonvm-es-suite-memorylayout-varhandle-20260710-032800-r6
```

Rows checked:

```text
others start=1659 count=1 org.elasticsearch.index.mapper.UpdateMappingTests
others start=1680 count=1 org.elasticsearch.index.query.CombineIntervalsSourceProviderTests
```

Both rows no longer show `DowncallHandle.invokeExact`, `MemoryLayout.varHandle`, `VarHandle.accessModeTypeUncached`, or `Invalid position 1 for coordinate types: [class java.lang.Object]`.

Current next blocker in both rows:

```text
java.lang.UnsatisfiedLinkError: Native library [/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch/lib/platform/linux-x64/libvec.so] does not exist
```

That `libvec.so` signal is a separate fixture/native-library issue and is already noted by the suite docs as not a clean CratonVM-only FFM failure.
