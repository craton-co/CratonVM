# crash-03: `PayloadUtilsTests` SIGSEGV (rc=139) in JIT-compiled code

| | |
|---|---|
| **Category** | **VM-CRASH** (SIGSEGV / `EXCEPTION_ACCESS_VIOLATION`, rc=139) — first true segfault of the run |
| **Module** | spring-messaging (rsocket) |
| **Test class** | `org.springframework.messaging.rsocket.PayloadUtilsTests` |
| **CratonVM** | CRASH rc=139 — `EXCEPTION_ACCESS_VIOLATION`, **write at address `0x0000000000000001`** |
| **HotSpot JDK 25** | **OK (8/8)** — confirmed CV-unique |
| **CratonVM HEAD** | `8e8e47d9` (suite run, stable binary) |
| **Status** | **FIXED + verified** on `fix/oom-array-alloc-abend` (`957270c8`) — `PayloadUtilsTests` 8/8 OK |
| **Suggested owner** | **me (fixed)** |

## Triage verdict (deterministic, CV-unique, NOT JIT)
- **HotSpot:** `PayloadUtilsTests` 8/8 OK → CV-unique.
- **CratonVM:** SIGSEGV **every run** (4/4), rc=139.
- **`--nojit`: STILL crashes** → **not a JIT miscompile** — it's a native/off-heap memory bug.
- The fault is a **write to address `0x1`** (near-null base+offset) in native code.

## Minimal repro (no Spring/rsocket — just Netty)
```java
ByteBuf b = io.netty.buffer.PooledByteBufAllocator.DEFAULT.buffer(64);  // <-- CratonVM SIGSEGVs here
b.writeBytes("sample data".getBytes());
```
CratonVM crashes on the **first `PooledByteBufAllocator` buffer write** (write @ `0x1`). For contrast,
on the same VM `Unpooled.buffer(...)` and even `PooledByteBufAllocator.DEFAULT.directBuffer(...)`
(`hasMemoryAddress=true`) both work, and HotSpot does all three. So the bug is specifically in
**CratonVM's handling of Netty's pooled-arena buffer** — almost certainly the `Unsafe` /
`PlatformDependent` memory-pool path (a pool chunk base address computed as ~0, so writes land at
`0x1`).

## Symptom
The batch JVM segfaults (`rc=139`, Windows `EXCEPTION_ACCESS_VIOLATION`) while running
`PayloadUtilsTests`. The fault is in **JIT-compiled code** — the crash dump lists JIT return
addresses and the shadow-stack header reads all-zero:
```
rip=0x00007FFF67061247
Code bytes preceding JIT return addresses:
  [frame1] @…E92D86: 89 84 24 A8 00 00 00 48 C7 44 24 30 00 00 00 00 0F 85 9E … 48 8D 4C 24 40 E8 7A F2 15 00
  [frame2] @…E92156: 0F 10 40 40 0F 11 41 40 81 A2 D0 00 00 00 1F 00 10 00 45 33 C0 49 8B D7 48 8B CB E8 5A 0B …
  [frame3] @…FE5AEE: 8B CC 48 81 C1 F0 04 00 00 48 8B D4 FF D0 … E8 62 C4 EA FF
Memory around R10 (…67050000): <unreadable>
ShadowStack @ R10+0x1B8: top=0 end=0 base=0
```
The `0F 10 40 40 / 0F 11 41 40` (movups xmm load/store at +0x40) + the all-zero ShadowStack header
suggest a JIT method operating on a receiver/struct via SSE moves where the base pointer (R10 shadow
stack) is unestablished — i.e. a JIT codegen/root-tracking fault, plausibly the same family as the
documented JIT shadow-stack / GC-root issues.

## Context
`PayloadUtilsTests` exercises RSocket `PayloadUtils` ↔ Netty `ByteBuf`/`DataBuffer` conversions
(off-heap direct buffers). A SIGSEGV here is either a JIT miscompile on that conversion code or an
off-heap/`Unsafe` buffer-handling bug.

## Reproduce
```bash
VM=C:/craton/spring-vm-stable/cratonvm.exe   # 8e8e47d9
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
CP="<harness>;$(tr -d '\r' < .../spring-messaging/build/cratonvm-testcp.txt)"
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.messaging.rsocket.PayloadUtilsTests
# triage:
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.messaging.rsocket.PayloadUtilsTests
# isolate the JIT culprit: re-run with --nojit; if it stops crashing, it's a JIT miscompile.
"$VM" --java-home "$JDK" --nojit -cp "$CP" KRun ...PayloadUtilsTests
```

## ROOT CAUSE — PINPOINTED + fix staged
Narrowing (one op per process):
```
pooled.directBuffer ALLOC ok        direct-WRITE -> SIGSEGV     heap pooled -> ok
pooled.directBuffer(64).memoryAddress() = 0x1   (HotSpot: 0x2e1b87da040)   [unpooled too]
```
Every direct buffer's `memoryAddress()` is **`0x1`** on CratonVM → the first write dereferences `0x1`.
Netty computes it via `PlatformDependent0.directBufferAddress(buf)` =
`UNSAFE.getLong(buf, objectFieldOffset(Buffer.address))`. Measured:
```
Unsafe.objectFieldOffset(Buffer.address) = 0x7ffffffffffffffe   (BUFFER_ADDRESS_SENTINEL)
Unsafe.getLong(buf, sentinel)            = 0x1                  (BUG; reflective Field.getLong = real addr)
```
`native-builtins/src/lib.rs` mints a **sentinel** offset for `objectFieldOffset(java/nio/Buffer.address)`
(so Netty's `PlatformDependent0.<clinit>` availability probe sees a "valid" offset), and
`native_unsafe_get_long` answered that sentinel with a **hardcoded `1`**. That was fine for the
*init probe* (just needs non-zero) but wrong for the **real** address reads Netty does for every direct
buffer — so `memoryAddress()` was always `1`.

**Fix:** `native_unsafe_get_long`, on the sentinel offset, now returns the receiver's **actual
`address` field** (`ctx.get_field_by_name(buf, "address")`, which is correctly populated — proven by a
reflective `Field.getLong`), falling back to `1` only when there is no native address. (Building +
verifying: `PooledByteBufAllocator.directBuffer().writeBytes()` + `PayloadUtilsTests`.)

## Notes
- Affects **all** Netty direct-buffer usage on CratonVM (rsocket, webflux/reactor, any off-heap path),
  not just this one test — high blast radius despite surfacing in a single class first.
- The crash filename says "jit" but triage proved it's **not** JIT (`--nojit` also crashed); it's the
  native `Unsafe`/off-heap path. Filename kept for link stability.
