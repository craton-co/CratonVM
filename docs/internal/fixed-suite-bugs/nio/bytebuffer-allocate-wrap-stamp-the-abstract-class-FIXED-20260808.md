# `ByteBuffer.allocate` / `wrap` stamp the abstract `java/nio/ByteBuffer`

| | |
|---|---|
| **Status** | **FIXED 2026-08-08.** `allocate` / `wrap` / `wrapSlice` and every heap view stamp the concrete `HeapByteBuffer` (`HeapByteBufferR` read-only). `NioBufferStampProbe` is byte-identical to HotSpot on all four rows. The dead S2 registrations are NOT removed — see below. |
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


## Fixed 2026-08-08

Confirmed reproducing against a HotSpot control first, then fixed. CratonVM is
now byte-identical to HotSpot on all four `NioBufferStampProbe` rows:

```text
allocate       kind=HeapByteBuffer   slice/dup=HeapByteBuffer  ro=HeapByteBufferR
wrap           kind=HeapByteBuffer   arrayOffset=0             ro=HeapByteBufferR
wrapSlice      kind=HeapByteBuffer   arrayOffset=8
allocateDirect kind=DirectByteBuffer ...                       ro=DirectByteBufferR
```

Every other column -- `order`, `rem`, `compact`, `put`, `getInt`, `eq`, `cmp`,
`hash`, `bulk`, `arrayOffset`, `address` -- is unchanged, which was the
requirement. `allocateDirect` is untouched.

`s2_bb_heap_class` picks the concrete name and falls back to the abstract
`java/nio/ByteBuffer` when only a synthetic stub is available, using the same
two-probe guard `allocateDirect` already used
(`would_fabricate_synthetic_stub` + `is_class_synthetic_stub`). That fallback
is the point of step 1 of the template above: a stub would trade
`AbstractMethodError` for a buffer whose every method is a silent no-op, which
is strictly worse than the status quo.

Step 2 needed no work: `s2_bb_new_heap_view` already seeded `address` as
`16 + offset` honestly, which is why `wrapSlice arrayOffset=8` was already
right before this change.

**A trap worth recording.** The first patch matched the alloc-site string
textually and rewrote **10** sites, two of which were `s2_bb_alloc_direct` and
`s2_bb_new_direct_view` -- DIRECT buffers, which have no backing array and must
never claim a `Heap*` class. It compiled clean. Enumerating the enclosing
function of every rewritten site caught it. The probe would also have caught it
on the `allocateDirect` row, but only because that row happens to exist.

## What is NOT done: the dead S2 registrations

The second half of this page's exit criteria is untouched, deliberately.

The S2 ByteBuffer natives are still registered on the abstract
`java/nio/ByteBuffer`, and a native on an abstract class still intercepts calls
on its concrete subclasses. So behaviour is unchanged by construction -- which
is exactly what makes this half safe to land on its own, and why the whole
behavioural column set above is identical.

It also means the registrations this page expected to become dead have NOT been
removed, and `--dump-native-registry` still shows them. Removing them is the
step that actually hands the ByteBuffer surface to real JDK bodies, and it is
the risky one this page warned about: `ByteBuffer` is reached by nearly
everything -- NIO, charset decode, every suite's I/O path. It wants its own
change with a suite run behind it.

What IS closed is the latent `AbstractMethodError` this page was filed for: a
method with no CratonVM native now dispatches to `HeapByteBuffer`'s real JDK
body instead of an abstract declaration with no Code attribute.

## Gates

`native-io` 435 lib tests pass; regression `RJdkNio` passes (81 checks). The
`cratonvm-native-builtins` LIB-TEST module does not compile on `dev` (622
errors, in unrelated code such as a digest assertion) -- pre-existing, not from
this change.
