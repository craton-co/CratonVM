# Bug 03 — operand-stack overflow panic in `Crc32CTest.testUpdate` (CRC32C dispatch)

**Severity:** High — process panic (`value_stack.rs:796`, "len 24 index 24").
`--nojit`, interpreter path. **CratonVM-only** (HotSpot runs it clean).

> Re-scoped from the original "common.utils panic": the package's `value_stack`
> panic was mis-attributed at first (the slow varint-loop tests, e.g.
> `testCorrectnessWriteUnsignedVarint` which loops `i` 0→2³¹, are a *throughput*
> issue and merely the first classes to time out). A per-test listener pinned the
> actual panic to **`Crc32CTest.testUpdate`**.

## Minimal repro (no JUnit)
```java
import java.util.zip.Checksum;
import org.apache.kafka.common.utils.Crc32C;
public class Crc5 {
  public static void main(String[] a){
    byte[] bytes = "Any String you want".getBytes(); int len = bytes.length;
    Checksum c1 = Crc32C.create(); c1.update(bytes,0,len);     // update([BII)V first
    Checksum c2 = Crc32C.create();
    for (int i=0;i<30;i++) c2.update(bytes[0]);                // update(I)V -> overflow
  }
}
```
- HotSpot: runs clean.
- CratonVM `--nojit`: `panic value_stack.rs:796 (len 24, index 24)` →
  `[PANIC_IN] Crc5.main pc=40 max_stack=4`.

## Root-cause localization

The panicking frame is `main` itself, whose declared `max_stack` is tiny (4–6),
yet its operand stack reaches **24** (= the pooled `max(max_stack,16)+8`). So
operands accumulate on the *caller's* stack — a leak, not a max_stack underestimate.

Key facts established with targeted tracing (`Checksum.update` is dispatched via
`execute_invoke_kind`, the slow path — `CRATONVM_DBG_UPDATE` trace):
- **Order-dependent:** the leak only occurs when a `Checksum.update([BII)V` call
  precedes the `update(I)V` calls. `update(I)V` in a loop *alone* (no prior
  `[BII`) runs clean.
- The slow-path arg accounting itself is correct per call site
  (`update([BII)` pops 4, `update(I)` pops 2 — both from the call-site descriptor).
- By the first `update(I)` the caller stack is already +1 over expected, and the
  overflow to 24 happens *during* the first `update(I)`'s callee chain —
  `Crc32C.create` → `CRC32C.<clinit>` → `java/nio/ByteOrder` / `jdk/internal/misc/Unsafe`,
  all executed as **"stackless" frames** (the dispatch-free interpreter
  optimization). The accumulation is in that stackless callee chain spilling onto
  the caller frame's operand stack rather than being confined to per-callee stacks.

## Root cause (FIXED)

The cached **`VirtualNative` fast path** (`invoke_cached_native_callback`) pushed the
native callback's return value **unconditionally** — even when the method's
descriptor return type is `V` (void). `java/util/zip/Checksum.update` is backed by a
native that returns `Some(_)` (natives commonly return the receiver / a status); the
slow path (`execute_invoke_kind`) correctly *drops* that value for a void method, but
the cached path pushed it, **leaking one operand per call**. The first call goes via
the slow path (correct) and primes the inline cache; every subsequent call takes the
leaking cached path — so the leak only appears once a `update` overload has primed the
cache, matching the order-dependence. The operands accumulate on the caller's frame
until they exceed its `max_stack` → `value_stack.rs` "len 24 index 24".

**Fix** (`vm/src/runtime/interpreter.rs`, `invoke_cached_native_callback`): only push
the native return value when `return_type != V`. This is a **general** correctness fix
— it affects any void-returning native reached through the cached VirtualNative path,
not just CRC32C.

Verified: `Crc5`/`Crc3` repros produce CRC values identical to HotSpot;
`Crc32CTest#testUpdate` passes; serialization/config packages unchanged (no
regressions).

## Status
- [x] Clean minimal repro; CratonVM-only.
- [x] Pinned to `Crc32CTest.testUpdate`; root-caused to the cached VirtualNative
      void-return push.
- [x] **Fixed** — cached path now drops void native returns (matches the slow path).

## Side note (not this bug, and HotSpot-shared)
`new java.util.zip.CRC32C()` directly returns null on CratonVM (NPE on use), and
`LoggingSignalHandlerTest` fails on **both** VMs (JDK25 `sun.misc.Signal` change) —
the latter is excluded as not CratonVM-only. CVM's synthetic `sun/misc/Signal` also
has 1 field vs the real 2 (`name`,`number`), producing benign field-OOB warnings.
