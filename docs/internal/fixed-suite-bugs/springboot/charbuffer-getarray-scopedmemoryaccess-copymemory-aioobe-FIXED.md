# `CharBuffer.getArray`'s `ScopedMemoryAccess.copyMemory` fast path throws `ArrayIndexOutOfBoundsException` on a heap-backed view — blocks `java.net.IDN`/ICU4X end-to-end

**Status: FIXED 2026-07-15** (reactive-cluster session, branch
`fix/reactive-cluster-20260715`). The doc's leading hypothesis was close
but the actual defect was one level up: `s2_bb_as_char_buffer`
(`native-builtins/src/servlet.rs`) never seeded the view's
`java.nio.Buffer.address` field, so the real `CharBuffer.getArray`
bulk path computed a source offset of `0 + (index << 1)` — below the
array-base offset (16) that `ScopedMemoryAccess.copyMemory`'s
array-offset decode requires — and threw AIOOBE. Fixed by seeding
`address = ARRAY_CHAR_BASE_OFFSET` (16), written after the indexed
fallback writes (BB_MARK aliases the real `address` slot), exactly
like `charset.rs::alloc_char_buffer` already did. Verified:
`IDN.toASCII("bücher.example")` == HotSpot (`xn--bcher-kva.example`),
bulk `CharBuffer.get(char[])` on an `asCharBuffer()` view returns
correct data, and the whole reactive Netty cluster this poisoned
(UCharacterProperty.<clinit> → ByteBufUtil NCDFE → RSocket/Reactor
Netty ABENDs) cleared. Known cosmetic divergence kept: the view
reports native byte order (its decoded char[] storage's true order)
while HotSpot reports the source ByteBuffer's order.

Original doc below.

---

**Status: OPEN.** Found 2026-07-14 while verifying the fix for
[`../../internal/springboot/charbuffer-order-missing-native-idn-clinit-cluster-FIXED.md`](native-idn-clinit-cluster-FIXED.md)
(that doc's `CharBuffer.order()` `AbstractMethodError` was masking this
issue entirely — with `order()` fixed, `java.net.IDN.<clinit>` now runs
further and hits this new, previously-unreachable exception instead of
succeeding).

## Symptom

Standalone repro (no Spring Boot needed):

```java
public class IdnRepro {
    public static void main(String[] args) throws Exception {
        String ascii = java.net.IDN.toASCII("example.com");
        System.out.println("IDN.toASCII result=" + ascii);
    }
}
```

```
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError class=jdk/internal/icu/impl/UCharacterProperty cause=java/lang/ArrayIndexOutOfBoundsException
  [CLINIT-TRACE 0] at IdnRepro.main (IdnRepro.java:3) bci=2
  [CLINIT-TRACE 1] at java/net/IDN.<clinit> (IDN.java:253) bci=42
  [CLINIT-TRACE 2] at jdk/internal/icu/text/StringPrep.<init> (StringPrep.java:228) bci=181
  [CLINIT-TRACE 3] at jdk/internal/icu/lang/UCharacter.getUnicodeVersion (UCharacter.java:419) bci=0
  [CLINIT-TRACE 4] at jdk/internal/icu/impl/UCharacterProperty.<clinit> (UCharacterProperty.java:630) bci=71
  [CLINIT-TRACE 5] at jdk/internal/icu/impl/UCharacterProperty.<init> (UCharacterProperty.java:597) bci=356
  [CLINIT-TRACE 6] at jdk/internal/icu/util/CodePointTrie.fromBinary (CodePointTrie.java:268) bci=443
  [CLINIT-TRACE 7] at jdk/internal/icu/impl/ICUBinary.getChars (ICUBinary.java:277) bci=9
  [CLINIT-TRACE 8] at java/nio/CharBuffer.get (CharBuffer.java:865) bci=5
  [CLINIT-TRACE 9] at java/nio/CharBuffer.get (CharBuffer.java:838) bci=39
  [CLINIT-TRACE 10] at java/nio/CharBuffer.getArray (CharBuffer.java:972) bci=104
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/ExceptionInInitializerError
	at IdnRepro.main(IdnRepro.java:3)
	at java/net/IDN.<clinit>(IDN.java:253)
	...
Caused by: java/lang/ArrayIndexOutOfBoundsException
	at IdnRepro.main(IdnRepro.java:3)
	at java/net/IDN.<clinit>(IDN.java:253)
	at jdk/internal/icu/text/StringPrep.<init>(StringPrep.java:228)
	at jdk/internal/icu/lang/UCharacter.getUnicodeVersion(UCharacter.java:419)
	at jdk/internal/icu/impl/UCharacterProperty.<clinit>(UCharacterProperty.java:630)
	at jdk/internal/icu/impl/UCharacterProperty.<init>(UCharacterProperty.java:597)
	at jdk/internal/icu/util/CodePointTrie.fromBinary(CodePointTrie.java:268)
	at jdk/internal/icu/impl/ICUBinary.getChars(ICUBinary.java:277)
	at java/nio/CharBuffer.get(CharBuffer.java:865)
	at java/nio/CharBuffer.get(CharBuffer.java:838)
	at java/nio/CharBuffer.getArray(CharBuffer.java:972)
```

This means `java.net.IDN.toASCII`/`toUnicode` still do not work end-to-end
on this build (the whole point of the sibling `order()` fix was to unblock
this path) — every Spring Boot class in the retired
`charbuffer-order-missing-native-idn-clinit-cluster` doc will still
FAIL/CRASH, just with this different exception instead of the old
`AbstractMethodError`/cascading `NoClassDefFoundError` chain.

## Analysis (not yet root-caused — hypothesis only)

Decompiled the real bytecode (`javap -p -c`) for the call chain:

- `ICUBinary.getChars(ByteBuffer, int, int)` does
  `buffer.asCharBuffer().get(char[])` — i.e. this goes through
  `ByteBuffer.asCharBuffer()` (`native-builtins/src/servlet.rs::s2_bb_as_char_buffer`),
  which allocates its returned view under the plain `java/nio/CharBuffer`
  class name and populates `hb`/`position`/`limit`/`capacity` by name (a
  heap char[] it fills by manually transcoding bytes — not a direct/native
  buffer).
- `CharBuffer.get(char[])` → `get(char[], int, int)` → the private bulk
  helper `getArray(int, char[], int, int)` (`CharBuffer.java:972`, matching
  the crash site above).
- `getArray`'s bytecode branches on `this.isAddressable()` — real
  `CharBuffer.isAddressable()` is **unconditional, always returns `true`**
  (`iconst_1; ireturn`, no per-subclass override needed/possible — it's not
  abstract). So the "addressable" fast path is *always* taken for every
  `CharBuffer`, not just direct ones.
- That fast path calls `jdk.internal.misc.ScopedMemoryAccess.copyMemory`
  (or `copySwapMemory` if `order() != nativeOrder()` — not the case here),
  passing `this.base()` (= `hb`, a concrete override returning the `hb`
  field directly — confirmed correctly populated by `s2_bb_as_char_buffer`)
  and a byte offset derived from `this.address` (the `Buffer.address` long
  field) plus `ARRAY_BASE_OFFSET`.
- **Leading hypothesis**: CratonVM's native `ScopedMemoryAccess.copyMemory`
  implementation likely does not correctly handle the "heap-relative
  offset" calling convention real `Unsafe`/`ScopedMemoryAccess.copyMemory`
  uses when a non-null `base` object is supplied (`address` is then a
  *relative* byte offset into that array, e.g. `ARRAY_BASE_OFFSET + i*2`,
  not an absolute native pointer) — possibly instead treating `address` as
  an absolute pointer regardless of `base`, or mis-deriving bounds for the
  destination `char[]` this repro's `s2_bb_as_char_buffer`-created view
  never explicitly initializes an `address` field for (defaults to 0,
  which is correct heap-buffer semantics, but may interact badly with
  whatever indexing `copyMemory`'s native performs).
- Not yet confirmed: whether this reproduces for a REAL (non-synthetic)
  heap `CharBuffer` too (e.g. plain `CharBuffer.wrap(new char[8])`,
  bypassing `asCharBuffer()` entirely) — that would isolate whether the bug
  is in `ScopedMemoryAccess.copyMemory` generally or specific to
  `s2_bb_as_char_buffer`'s view construction (e.g. a missing/incorrect
  `address` or `capacity` field on that particular synthetic object).

## Repro

```java
public class IdnRepro {
    public static void main(String[] args) throws Exception {
        System.out.println(java.net.IDN.toASCII("example.com"));
    }
}
```

```powershell
javac IdnRepro.java
target\release\cratonvm.exe -c . IdnRepro
```

Reproduces the `ArrayIndexOutOfBoundsException` above on this worktree's
current build (`feat/spring-boot-crashfail-20260714`, dev tip `1021533f9`
plus the `CharBuffer.order()` and `Map.Entry::getKey` fixes from the same
session).

## Related

- [`../../internal/springboot/charbuffer-order-missing-native-idn-clinit-cluster-FIXED.md`](native-idn-clinit-cluster-FIXED.md)
  — the sibling `CharBuffer.order()` bug this residual was hiding behind;
  fixing that one is what exposed this one.
- `native-builtins/src/servlet.rs::s2_bb_as_char_buffer` — the view
  constructor whose returned object hits this path first in the observed
  trace.
- Not yet investigated: the `ScopedMemoryAccess.copyMemory`/`copySwapMemory`
  native implementations themselves (wherever they're registered — not
  located in this pass).
