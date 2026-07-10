# `ByteBuffer.allocate()` never sets `Buffer.address` → AIOOBE on every bulk `get(byte[])`/`put(byte[])`

**Status:** FIXED 2026-07-09 - the live default-release allocator was
found and now seeds `java.nio.Buffer.address = 16` for heap buffers returned by
`ByteBuffer.allocate(int)`. **Severity: HIGH** - this previously broke real-net
NIO servers that read incoming bytes into a `byte[]` via bulk
`ByteBuffer.get(byte[])`/`put(byte[])`, e.g. Tomcat's `NioEndpoint`.
**HotSpot:** unaffected (n/a - this was a CratonVM-only synthetic-object gap).

Found 2026-07-09 while re-verifying
[`accesslogvalve-rewritevalve-connection-failures.md`](../../known-issues/tomcat-08-07/accesslogvalve-rewritevalve-connection-failures.md).

## Symptom

Any `ByteBuffer` obtained via `ByteBuffer.allocate(n)` throws
`ArrayIndexOutOfBoundsException` the first time a bulk method
(`get(byte[])`, `put(byte[])`, or anything else that routes through
`jdk.internal.misc.ScopedMemoryAccess.copyMemory`) touches it — even for a
perfectly in-bounds, freshly-filled buffer:

```
Exception in thread "main" java.lang.ArrayIndexOutOfBoundsException
	at ServerOnlyProbe2.main(ServerOnlyProbe2.java:25)
	at java.nio.ByteBuffer.get(ByteBuffer.java:865)
	at java.nio.ByteBuffer.get(ByteBuffer.java:838)
	at java.nio.ByteBuffer.getArray(ByteBuffer.java:972)
	at jdk.internal.misc.ScopedMemoryAccess.copyMemory(ScopedMemoryAccess.java:130)
	at jdk.internal.misc.ScopedMemoryAccess.copyMemoryInternal(ScopedMemoryAccess.java:148)
```

Practical impact: a real `SocketChannel.read(ByteBuffer)` into an
`allocate()`d buffer, followed by the idiomatic
`byte[] arr = new byte[n]; buf.flip(); buf.get(arr);` pattern, crashes the
reading thread instantly. For an NIO server (Tomcat's `NioEndpoint`, or any
other Java server) this typically kills a background acceptor/poller thread
silently — the client's request socket just hangs / gets reset with no
response, which is why this surfaced upstream as connection-level `-1`
response codes in the Tomcat suite, not as a visible crash.

## Root cause

`java.nio.Buffer.address` (a `long`, inherited by every `ByteBuffer`) is
**never written** by whatever creates the object behind
`ByteBuffer.allocate(int)` under CratonVM. `getClass().getName()` on such a
buffer reports the literal abstract class `java.nio.ByteBuffer` (HotSpot:
`java.nio.HeapByteBuffer`) — confirming it's a synthetic carrier object, not
one built via the real `HeapByteBuffer` constructor chain (which would have
set `address = ARRAY_BYTE_BASE_OFFSET + offset` = 16 for a fresh buffer).

Real bulk-transfer bytecode
(`HeapByteBuffer.get(byte[],int,int)` → `getArray` →
`ScopedMemoryAccess.copyMemory(srcBase, srcOffset, dstBase, dstOffset,
bytes)`) computes `srcOffset = address + position`. With `address` stuck at
its uninitialized default, `srcOffset` comes out `0` (confirmed via a debug
trace on `Unsafe.copyMemory`'s consolidated handler — see below) instead of
the correct `16`. `native-builtins/src/lib.rs::native_unsafe_copy_memory`'s
heap-array path (correctly) does
`byte_off.checked_sub(ABASE /* 16 */)` to convert a real-JDK
base-offset-relative index back to a plain array index; for the
**destination** array (a tightly-sized `new byte[n]`, `dstOffset = 16 + 0 =
16` correctly), `16 + n > n` fails the bounds check → the whole
`Unsafe.copyMemory` throws `ArrayIndexOutOfBoundsException` (HotSpot's
documented behavior for a genuinely out-of-bounds copy — the object itself
is behaving "correctly" given the bad `address` value it was seeded with).

Confirmed via a debug trace directly inside
`native_unsafe_copy_memory` (`native-builtins/src/lib.rs`):
```
[COPYMEM-DBG] src_offset=0 dest_offset=16 bytes=85
```
`src_offset=0` is the tell — a real `HeapByteBuffer`'s `address` field would
make this `16` (matching `dest_offset`).

## Minimal repro (no Tomcat, no HTTP — pure NIO)

```java
// Server: CratonVM (real-net-sockets), reads via SocketChannel + bulk get(byte[])
ServerSocketChannel serverCh = ServerSocketChannel.open();
serverCh.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
SocketChannel ch = serverCh.accept();
ByteBuffer buf = ByteBuffer.allocate(4096);
int n = ch.read(buf);                 // succeeds, n=85, buf state is internally
                                        // consistent (pos=85/limit=4096, flip->pos=0/limit=85)
byte[] arr = new byte[n];
buf.flip();
buf.get(arr);                          // <-- AIOOBE here, 100% reproducible
```
Run against a **trusted external client** (a plain Python `socket` sending a
raw, well-formed request) to rule out any bug in the test's own client code —
confirmed the client side is fine; the server-side `buf.get(arr)` throws
regardless of what sent the bytes.

A companion probe confirms `ByteBuffer.wrap(byte[])` — unlike `allocate()` —
correctly returns `java.nio.HeapByteBuffer` (byte-identical to HotSpot),
i.e. `wrap()` takes a different (working) path than `allocate()`.

`BufAddrProbe` (reflectively reads `Buffer.address` via `Field.getLong`)
consistently shows `address=-1` for any `allocate()`d buffer, both before and
after every fix attempt below.

## Historical investigation dead-ends

Two natural-looking fix locations were tried, both proven **not** to be the
live dispatch path via an *unconditional* `eprintln!` placed directly inside
the registered closure (rebuilt, rerun, zero output — not gated on any env
var, so this is conclusive, not a logging mistake):

1. **`native-builtins/src/servlet.rs::bb_write_hb`** (used by `s2_bb_alloc`,
   which backs the `java/nio/ByteBuffer allocate (I)Ljava/nio/ByteBuffer;`
   registration in the same file, category `SyntheticStub`). This
   registration **is** unconditionally reached at boot (confirmed via
   `--dump-native-registry`, which lists it) and `register_s2_nio()` (its
   caller) is not feature-gated — yet the closure never runs for an actual
   `ByteBuffer.allocate()` call in a plain `cargo build --release` (no
   `--features synthetic-jdk`) binary.
2. **`native-io/src/lib.rs::alloc_byte_buffer`** (used by `native_bb_allocate`,
   registered for both `java/nio/ByteBuffer` and `java/nio/HeapByteBuffer`,
   category `Bridge`) — this one already has the CORRECT 3-field-aware
   shape and is a better implementation, but its caller chain
   (`register_nio_natives` → `register_io_natives`) is called from
   `vm/src/vm/vm_init.rs` **only** inside
   `#[cfg(feature = "synthetic-jdk")] { if config.use_synthetic_jdk { ... } }`
   — and `synthetic-jdk` is explicitly **not** in this crate's default
   feature set (see `vm/Cargo.toml`'s "NEW-11" comment). A plain release
   build compiles this registration out entirely. Matches the previously
   documented [[reference_synthetic_jdk_feature_gate_trap]] pattern exactly.

Those attempted fixes were correctly reverted during investigation because they
were not the live default-release allocator. The same `address = 16` write is
now applied at the actual live site (`native-builtins/src/lib.rs`) and also in
`native-io/src/lib.rs::alloc_byte_buffer` so the feature-gated bridge path stays
consistent.

`classloading/src/class_manager.rs` (around line 10685) explains *why*
`allocate()` even goes through native dispatch instead of running real
`new HeapByteBuffer(cap, cap)` bytecode: it unconditionally appends a
`MethodAccessFlags::NATIVE`-flagged, no-`Code`-attribute `allocate`/
`allocateDirect` declaration onto whatever classfile gets loaded for
`java/nio/ByteBuffer` — this exists (per a nearby comment referencing
`CRATONVM_JIT_BISECT_SKIP` / `vm/src/jit/skip_list.rs`) to route around a
**separate**, already-diagnosed JIT/GC allocation-overlap corruption bug in
`ByteBuffer.allocate`'s real constructor path (`BUG-D`-family). So real
bytecode is deliberately avoided for this method — but *something* still
answers the native call with a plausible-but-broken object, and that
something is not `shared.native_methods.find(...)`'s normal registry lookup
(or if it is, the winning entry could not be found by any of the exhaustive
class/method/descriptor greps performed in this investigation).

## Fix

The live default-release dispatch site is
`native-builtins/src/lib.rs::native_heap_bytebuffer_allocate`, registered by
`register_essential_natives` as a `SyntheticStub` for
`java/nio/ByteBuffer.allocate(I)Ljava/nio/ByteBuffer;`. Its helper
`alloc_heap_bytebuffer` wrote `hb`, `offset`, `position`, `limit`, `capacity`,
and `mark`, but never wrote inherited `Buffer.address`.

The fix writes `ctx.set_field_by_name(buf, "address", Value::Long(16))` last,
matching HotSpot's `ARRAY_BYTE_BASE_OFFSET + offset` for a fresh heap byte
buffer. Writing it last is important because the synthetic compatibility slots
can overlap the real-JDK `Buffer.address` field layout.

`native-io/src/lib.rs::alloc_byte_buffer` now does the same for the bridge
allocator, even though that path is not the default-release dispatch path, so
future synthetic/bridge runs keep the same invariant.

## Validation

Validated in worktree
`/data/data/cratonvm-worktrees/20260709-bytebuffer-address-aioobe-001` with a
unique release target directory and unique probes:

- `CARGO_TARGET_DIR=/data/data/target-bytebuffer-address-aioobe-20260709-001 cargo test -p cratonvm-native-builtins bytebuffer_allocate_initializes_real_address_for_bulk_copy -- --nocapture`
- `CARGO_TARGET_DIR=/data/data/target-bytebuffer-address-aioobe-20260709-002 cargo test -p cratonvm-native-io allocated_heap_bytebuffer_sets_real_address -- --nocapture`
- `CARGO_TARGET_DIR=/data/data/target-bytebuffer-address-aioobe-20260709-release cargo build --release --bin cratonvm`
- `ServerOnlyAddressProbe20260709A` in `/data/data/bytebuffer-address-aioobe-serverprobe-20260709-001`: CratonVM accepted a localhost HTTP request, executed `SocketChannel.read(ByteBuffer.allocate(...))`, `flip()`, bulk `get(byte[])`, `clear()`, and returned `HTTP/1.1 200 OK` with body `OK`.

Full workspace `cargo fmt --check` was not used as a gate because current `dev`
already reports unrelated formatting drift in `jit/src/ir_lower.rs` and
`vm/src/vm/vm_util.rs`.
