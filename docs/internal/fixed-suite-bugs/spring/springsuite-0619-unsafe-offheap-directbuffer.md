<!-- One file per CratonVM-unique crash/hang/correctness root cause. -->
# bug-A: `Unsafe.put/getX(long)` rejects real DirectByteBuffer addresses → off-heap DataBuffers broken

| | |
|---|---|
| **Category** | VM-CORRECTNESS (off-heap memory) |
| **Module** | spring-core |
| **Test class(es)** | `core.io.buffer.PooledDataBufferTests`, `DataBufferUtilsTests`, `LeakAwareDataBufferFactoryTests` |
| **Failing test(s)** | `retainAndRelease()`, `tooManyReleases()`, + most off-heap buffer ops |
| **CratonVM** | FAIL — `IllegalArgumentException: Unsafe.putByte: address 0x… is not in any live arena` (+ `ArrayIndexOutOfBoundsException`) |
| **HotSpot JDK 25** | OK (PooledDataBufferTests 10/10) |
| **CratonVM HEAD** | found `c4536b94`; **FIXED on dev `3b16e985`+** (this session) |
| **Status** | ✅ **FULLY RESOLVED** (re-verified 2026-06-20, dev `697134f8`) — bug-A AND the bug-A2 residual both pass now: `PooledDataBufferTests` **10/10**, `LeakAwareDataBufferFactoryTests` **2/2**, `DefaultDataBufferTests` **1/1**. Archived here. |
| **Suggested owner** | done (landed on dev) |

> **RE-VERIFY 2026-06-20** (build `cratonvm-spring0620` off dev `697134f8`, JDK 25, via the
> JUnit-Platform `KRun` harness): `core.io.buffer.PooledDataBufferTests` now passes **10/10**
> (the bug-A2 `retainAndRelease()`/`tooManyReleases()` `ArrayIndexOutOfBoundsException` is **gone**),
> `LeakAwareDataBufferFactoryTests` **2/2**, `DefaultDataBufferTests` **1/1**. Note: running these
> classes at all first required the separate `ReferencePipeline.toArray(IntFunction)` recursion fix
> (commit `8795b88d`) — without it the JUnit launcher `StackOverflowError`'d before any DataBuffer
> test executed. With both fixes in place bug-A2 no longer reproduces, so this doc (bug-A + A2) is
> moved to the fixed-suite archive. (`DataBufferUtilsTests` still TIMEOUTs — a separate
> heavy-reactive issue, not off-heap addressing.)

> **FIX (landed):** the 8 single-element `Unsafe.{get,put}{Byte,Short,Int,Long}` at-address
> natives in `../../../../native-builtins/src/unsafe_natives.rs` now fall through to a raw real-memory
> access (`NativeContext::copy_{from,to}_native_memory`) for **untagged** addresses (real
> `DirectByteBuffer` pointers), via the new `real_ptr_read`/`real_ptr_write` helpers and the
> `unsafe_arena_addr_is_tagged` classifier in `lib.rs`. **Tagged** (live or freed) arena
> handles are excluded, so the use-after-free `IllegalArgumentException` guard is preserved.
>
> **Verified on the fixed binary:** the `Unsafe … not in any live arena` IAE is **gone**;
> `LeakAwareDataBufferFactoryTests` now passes (was FAIL) and `PooledDataBufferTests` goes
> 2/10 → 6/10. Heap `DefaultDataBufferTests`/`JettyDataBufferTests` stay OK (no regression).
>
> **Residual — bug-A2 (separate, OPEN):** the remaining `PooledDataBufferTests` failures
> (`retainAndRelease()`, `tooManyReleases()`) are now `ArrayIndexOutOfBoundsException` in
> Netty's reference-count `retain/release` path — a **distinct** defect (likely the
> `AtomicIntegerFieldUpdater`-backed `refCnt` updater), not the arena-addressing bug fixed here.

## Symptom
The off-heap (Netty pooled / direct) `DataBuffer` classes fail; the heap-backed ones pass:

| Class | CV | Backing |
|---|---|---|
| `DefaultDataBufferTests` | **OK** | heap `byte[]` |
| `PooledDataBufferTests` | **FAIL 2/10** | Netty pooled **direct** (off-heap) |
| `DataBufferUtilsTests` | FAIL | mixed, exercises direct |
| `LeakAwareDataBufferFactoryTests` | FAIL | wraps direct |

Representative failures (CratonVM, HotSpot passes all):
```
RESULT …PooledDataBufferTests found=10 succ=2 fail=8 status=FAIL
FAILCAUSE …PooledDataBufferTests :: retainAndRelease() :: java.lang.ArrayIndexOutOfBoundsException: null
FAILCAUSE …PooledDataBufferTests :: tooManyReleases() :: java.lang.ArrayIndexOutOfBoundsException: null
FAILCAUSE …PooledDataBufferTests :: retainAndRelease() :: java.lang.IllegalArgumentException:
          Unsafe.putByte: address 0x1de9bafa8a0 is not in any live arena
```

## Root cause (confirmed from source + the failing address)
CratonVM models `Unsafe.allocateMemory` as a **synthetic tagged "arena" store**
(`../../../../native-builtins/src/lib.rs` `mod unsafe_arena`). Every arena handle has bit 62
(`ARENA_TAG = 1<<62`) set so handles are provably disjoint from real OS pointers.
The single-element accessors —
`native_unsafe_put_byte_at_address` / `…_get_byte_at_address` and the short/int/long
variants in `native-builtins/src/unsafe_natives.rs:341` — resolve the address **only**
through this arena store and throw `IllegalArgumentException("… not in any live arena")`
when it isn't a live tagged handle.

But the failing address proves the input is **not** an arena handle:

```
0x1de9bafa8a0  =  0x0000_01de_9baf_a8a0
bit 62 (0x4000_0000_0000_0000) = CLEAR  →  UNTAGGED  →  a real pointer
```

That is exactly a `DirectByteBuffer` address minted by `dbb_allocate` (the arena code's
own comment at `lib.rs:15633` calls this out: *"a real pointer e.g. a
`ByteBuffer.allocateDirect` address from `dbb_allocate`"*). Netty's pooled/direct
`DataBuffer` obtains its direct-buffer base address and writes through
`Unsafe.putByte(address, b)` — so every such write is wrongly rejected.

The **bulk** path already handles this correctly: `copyMemory` has a real-pointer
"falls through to a raw copy" branch (`unsafe_natives.rs:800`). The **single-element**
put/get natives never got that branch — so bulk ops pass (the 2/10 that survive) and
byte/short/int/long element ops on direct buffers fail.

## Reproduce
```bash
VM=/c/craton/spring-vmbin/cratonvm-spring0618.exe
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
CP="<spring-suite>;<spring-core/build/cratonvm-testcp.txt>"
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.core.io.buffer.PooledDataBufferTests
# HotSpot (passes 10/10):
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.core.io.buffer.PooledDataBufferTests
```

## Minimal trigger (expected)
```java
ByteBuffer bb = ByteBuffer.allocateDirect(16);
long addr = ((sun.nio.ch.DirectBuffer) bb).address();   // untagged real pointer
Unsafe.getUnsafe().putByte(addr, (byte) 7);             // CV: IAE "not in any live arena"
```

## Fix direction
Give the single-element `put*/get*_at_address` natives the **same real-pointer
fallthrough** the `copyMemory` path has: when `addr & ARENA_TAG == 0` (untagged), route
to the real address via `unsafe_arena_raw_ptr`/raw `ptr::read_volatile`/`write_volatile`
instead of throwing. (Alternatively, register `dbb_allocate` direct-buffer ranges in the
arena store — heavier.) The `ArrayIndexOutOfBoundsException: null` failures are the
downstream manifestation of the same rejected access and should clear with the fix.

## Notes
- Heap `DefaultDataBuffer` is unaffected — confirms the off-heap/Unsafe boundary.
- Related VM area: `../../../../native-builtins/src/unsafe_natives.rs`, `lib.rs mod unsafe_arena`.
- Blast radius beyond spring-core: any Netty/off-heap user (reactor-netty, webflux
  codecs). Several `ResourceRegionEncoderTests`-style reactive TIMEOUTs may be downstream.
