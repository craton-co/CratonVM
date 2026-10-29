# Checked scalar ScopedMemoryAccess intrinsics, 2026-10-04

Status: IMPLEMENTED; final merged-source validation remains pending with the
Netty retirement work in
[`jit-only-null-receiver-readerindex-and-timeouts-FIXED-20261012.md`](../../internal/fixed-suite-bugs/netty/jit-only-null-receiver-readerindex-and-timeouts-FIXED-20261012.md).

The public byte, short, char, int and long accessors remain Java bytecode.
Only their aligned/unaligned Internal operations are explicit reviewed
`NativeKind::Intrinsic` registrations, in both runtime modes. The earlier
retirement tables measured the old callbacks and their outer/inner pairs.
Their fourteen scalar Internal entries are removed from those tables;
copyMemory and public wrappers keep their prior policy. The strict-boot
ratchet continues to reject every surviving entry in a retirement table.

Each callback opens the shared-close access window before the authoritative
session validation, checks this VM's RawMemory capability, performs the
corresponding Unsafe operation with the exact width/order/sign behavior, and
closes the window after the access. Failing and unfamiliar sessions retain
Java's check and exception behavior. Arguments remain native-frame roots
throughout the operation. The VM's direct helpers serve only their own
selected intrinsic gate, a validated session and native-owned carrier proof;
mutable scope fields and lifetime/layout/VM epochs are checked again on proof
reuse. No compatibility process global is added.

`vm/tests/scoped_byte_access.rs` and `scoped_wide_access.rs` compare exact
HotSpot output in both modes, with JIT on and off: heap/direct/FFM buffers,
unaligned offsets, both byte orders, signed boundaries, char unsigned values,
float/double raw bits, read-only and bounds errors, confinement, aliases,
slices, GC, and close races. The byte/VarHandle probes also passed twelve
G1/Generational/ZGC stress arms before the final compiler change; the final
candidate still owes those collector controls. Registry classification is
checked by `native-builtins/tests/scoped_memory_intrinsic_kinds.rs`.

Release candidate twelve passes the complete JdkZlibIntegrationTest, 11/11,
in 98.630 seconds with the original 120-second method deadline. Both-mode
repeat and final merged-source tests remain acceptance gates. No broader
Netty retirement or delivery is claimed here.

2026-10-07 (round 19, `r16-ffm-plain-downcall-costs-microseconds`): `copyMemoryInternal`
joins the scalar Internal rows as a reviewed `Intrinsic` (its `copyMemory` wrapper stays
Java and retired). As JDK bytecode its bulk copy held no shared-close window, so an
`Arena.ofShared().close()` on another thread freed the block under an in-flight
`MemorySegment.copy` (`Unsafe.copyMemory: src address .. is not addressable`,
`R17Ffm2Direct`); the callback opens the window over both session checks and the copy and
checks the RawMemory capability, as the scalar ones do.
`native-builtins/tests/scoped_memory_intrinsic_kinds.rs` asserts the classification.
