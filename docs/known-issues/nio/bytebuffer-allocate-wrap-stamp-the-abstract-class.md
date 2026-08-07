# `ByteBuffer.allocate` / `wrap` stamp the abstract `java/nio/ByteBuffer`

| | |
|---|---|
| **Status** | OPEN — latent, and that is a measurement, not an assumption |
| **Severity** | low today, unbounded on contact — every method nobody has written a native for is an `AbstractMethodError` waiting for its first caller |
| **Split from** | `fixed-suite-bugs/nio-buffer-address-indexed-slot-aliasing-FIXED.md`, whose own subject (the `address`/`mark` slot aliasing) is fixed and verified. This is the one item that record deliberately did not sweep. |
| **Probe** | `probes/NioBufferStampProbe` — it already prints the `kind=` line that will flip |

## The divergence

```text
HotSpot   allocate  kind=HeapByteBuffer   ro=kind=HeapByteBufferR
CratonVM  allocate  kind=ByteBuffer       ro=kind=ByteBuffer
```

`java.nio.ByteBuffer` is abstract. CratonVM's allocator stamps that abstract
class onto the instance it hands back, so any real-JDK method *without* a
CratonVM native dispatches to its abstract declaration and throws
`AbstractMethodError: … has no Code attribute`. Nothing does today because
`register_essential_natives` registers the S2 ByteBuffer surface in real-JDK
mode precisely so those abstract methods have bodies — which is why the
exposure is conditional rather than broken.

**Measured 2026-08-07** (`--real-jdk`, JDK 25, `probes/NioBufferStampProbe`
against a HotSpot control): every method the probe calls behaves identically to
HotSpot — `slice`, `duplicate`, `asReadOnlyBuffer` (read-only IS enforced:
`put` throws `ReadOnlyBufferException`, `array()` throws), `compact`, `getInt`,
`equals`/`compareTo`/`hashCode`, bulk `put(Buffer)`, `mismatch`, `alignedSlice`,
`slice(int,int)`, `get(int,byte[])`, `StandardCharsets.UTF_8.decode(bb)`, and
`address` on both a plain `wrap` and a `wrapSlice`. The only divergence is
`getClass()`.

`allocateDirect` is already correct (`kind=DirectByteBuffer`,
`ro=kind=DirectByteBufferR`), as is the whole CharBuffer family
(`kind=HeapCharBuffer` / `HeapCharBufferR`, and `asCharBuffer` →
`ByteBufferAsCharBufferB`/`RB`).

## Why it was not swept with the CharBuffer half

Stamping `HeapByteBuffer` is the right end state, but it hands the entire
ByteBuffer surface over to real JDK bodies **at once**, and `ByteBuffer` is
reached by nearly everything — NIO, charset decode, every suite's I/O path. The
CharBuffer half of the same defect was fixed and is the template for doing it:

1. build the concrete class (`ensure_class_initialized`, then check
   `is_class_synthetic_stub` — `ensure_class_initialized` fabricates a stub
   rather than failing, and a stub would trade `AbstractMethodError` for a
   buffer whose every method is a silent no-op);
2. seed `address` honestly (`ARRAY_BYTE_BASE_OFFSET + offset`, not a constant —
   a slice carries a larger address);
3. delete the overrides that then only shadow the JDK's own bodies.

## Exit criteria

`NioBufferStampProbe`'s `allocate` / `wrap` / `wrapSlice` rows print
`kind=HeapByteBuffer` (and `kind=HeapByteBufferR` for the read-only view),
with every other column unchanged, and the `--dump-native-registry` census
shows the S2 ByteBuffer registrations that become dead actually removed rather
than left shadowing.
