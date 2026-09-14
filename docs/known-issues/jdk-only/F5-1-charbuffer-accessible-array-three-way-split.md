# F5-1 — `CharBuffer.hasArray/array/arrayOffset` had ONE branch where the JDK has THREE

> **RECONCILED 2026-08-17 (lane G40) — the reachability argument in §"the
> `--jdk-only` question" is VOID. The three-way split itself is untouched.**
>
> This record argues that because `java/nio/CharBuffer` is in neither
> `force_native_over_real_jdk_bytecode`'s list nor the `check_override` name
> chain, *"in real-JDK mode these natives answer for exactly the receivers whose
> resolved method has no `Code`"*. **The force list is not the gate.** `G34-1`
> measured it on a real binary in both directions, cold and warm: under
> `--jdk-only`, registering a `Bridge` for a triple is **by itself sufficient**
> to preempt real JDK bytecode. The decision is taken at the first dispatch site
> that answers, and for nearly every call that is `try_stackless_invoke` step 1 →
> `resolve_step1_native`, which runs *before* method resolution and passes
> `bytecode_available: false` unless `CRATONVM_ENFORCE_NATIVE_SHADOW` is armed.
> The force list is a **second, later, cache-shape-only** override, consulted
> only by the vtable inline cache and the JIT. The answer is therefore
> **site**-dependent, not triple-dependent.
>
> **What this banner does NOT claim.** It does not say this record's conclusion
> about the `CharBuffer` family is wrong — only that the argument for it is void.
> Re-deriving the answer needs a registry dump and a behavioural probe on the
> current binary, from a lane that may edit Rust. That was not done. See
> `INDEX.md` §B.2 and §D.1.

**2026-08-13, lane F5.** Fixes the `java.nio.CharBuffer` half of the
"accessible backing array" contract in
`native-builtins/src/phases_late/charset_buffers.rs`
(`register_p62_char_buffer`), plus the read-only refusal on `put(char)` and the
read-only bit `subSequence` was dropping.

**Provenance: MEASURED oracle, PREDICTED VM.** Every expected value below is a
pasted transcript from `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft
build) on this host, produced by the new
`probes/BufferAccessibleArrayProbe.java` (72 rows) and a second ordering probe.
**This lane may not build or run CratonVM**, so every "after" is a prediction
and is labelled as one. §6 gives the discriminator that tells the measuring
party, from the failure line alone, whether this fix is the whole answer.

**File chosen: `native-builtins/src/phases_late/charset_buffers.rs`.** The brief
deliberately named no path. `java/nio/CharBuffer.array()[C`, `hasArray()Z` and
`arrayOffset()I` are registered in exactly two places — here (`p62`) and
`native-io/src/lib.rs`'s `register_nio_natives` — and the second is
`#[cfg(feature = "synthetic-jdk")]` *and* skipped whenever
`registry.drops_real_layout_synthetic()`, which `vm/src/vm/vm_init.rs` sets on
**both** real-JDK arms (L2013, L2577). So in real-JDK mode this file is the only
registrar. `native-io/src/lib.rs` is also a `lib.rs` and therefore out of this
lane's ownership either way; its copy is §7 N1.

---

## 1. The contract, from `jdk25src`

`java.base/java/nio/CharBuffer.java` L1490 / L1513 / L1541 — and the
byte-identical bodies in `ByteBuffer.java` at the *same three line numbers*, both
generated from `X-Buffer.java.template`:

```java
public final boolean hasArray() { return (hb != null) && !isReadOnly; }

public final char[] array() {
    if (hb == null)  throw new UnsupportedOperationException();
    if (isReadOnly)  throw new ReadOnlyBufferException();
    return hb;
}

public final int arrayOffset() {          // the identical split
    if (hb == null)  throw new UnsupportedOperationException();
    if (isReadOnly)  throw new ReadOnlyBufferException();
    return offset;
}
```

`hasArray() == false` therefore covers **two** states that raise **different,
unrelated** classes. `java.lang.UnsupportedOperationException` and
`java.nio.ReadOnlyBufferException` share no supertype below `RuntimeException`,
so no `catch` can absorb the difference. **The order is the specification**: a
`StringCharBuffer` is read-only AND array-less at once, and the JDK asks
"array-less?" first — so it answers `UnsupportedOperation`. An implementation
that checks read-only first gets that one cell wrong and every other cell right.

### 1.1 Measured, `probes/BufferAccessibleArrayProbe`, jdk-25.0.3+9

```
scb.getClass	OK java.nio.StringCharBuffer
hcb.getClass	OK java.nio.HeapCharBuffer
rcb.getClass	OK java.nio.HeapCharBufferR
scb.isReadOnly	OK true
hcb.isReadOnly	OK false
rcb.isReadOnly	OK true
scb.hasArray	OK false
hcb.hasArray	OK true
rcb.hasArray	OK false
scb.array	java.lang.UnsupportedOperationException
hcb.array	OK 4
rcb.array	java.nio.ReadOnlyBufferException
scb.arrayOffset	java.lang.UnsupportedOperationException
hcb.arrayOffset	OK 0
rcb.arrayOffset	java.nio.ReadOnlyBufferException
```

`scb` = `CharBuffer.wrap("abcd")`, `hcb` = `CharBuffer.allocate(4)`,
`rcb` = `hcb.asReadOnlyBuffer()`.

The ByteBuffer side, same probe, adds the fourth receiver shape and confirms the
split is about `hb` and not about "directness":

```
hbb.hasArray	OK true      hbb.array	OK 8                                   (HeapByteBuffer)
dbb.hasArray	OK false     dbb.array	java.lang.UnsupportedOperationException (DirectByteBuffer)
rbb.hasArray	OK false     rbb.array	java.nio.ReadOnlyBufferException       (HeapByteBufferR)
rdb.hasArray	OK false     rdb.array	java.lang.UnsupportedOperationException (DirectByteBufferR)
```

`rdb` is read-only **and** direct and answers `UnsupportedOperation` — the same
cell `scb` occupies, and the second independent witness that array-less is
checked first.

### 1.2 The cell that says the split is not "read-only vs not"

```
vib.getClass	OK java.nio.ByteBufferAsIntBufferB
vib.hasArray	OK false
vib.array	java.lang.UnsupportedOperationException
rvib.isReadOnly	OK true
rvib.array	java.lang.UnsupportedOperationException
rib.getClass	OK java.nio.HeapIntBufferR
rib.array	java.nio.ReadOnlyBufferException
```

`vib` is a **writable** view buffer that still throws `UnsupportedOperation`, and
`rvib` is a **read-only** view that throws `UnsupportedOperation` rather than
`ReadOnlyBuffer` — because `ByteBufferAsCharBufferRB` &c. override the
`isReadOnly()` METHOD and never write the `isReadOnly` FIELD (JDK 25
`ByteBufferAsCharBufferRB.java:214` is the only `isReadOnly` in that file). The
field-vs-method distinction is real, and reading the field is what the JDK's own
bodies do. `HeapCharBufferR`/`HeapIntBufferR` DO write the field
(`HeapCharBufferR.java:83, 97, 114`).

### 1.3 Read-only outranks BOTH bounds classes

Second probe, same JDK, mutators on a receiver that is read-only *and* out of
room / out of range:

```
fullRo.put('z')   rel (readonly + overflow)   java.nio.ReadOnlyBufferException
full.put('z')     rel (writable + overflow)   java.nio.BufferOverflowException
fullRo.put(9,'z') abs (readonly + oob)        java.nio.ReadOnlyBufferException
scb.put(9,'z')    abs (readonly + oob)        java.nio.ReadOnlyBufferException
emptyRo.compact()                             java.nio.ReadOnlyBufferException
fullRbb_ro.put((byte)2) rel (readonly+overflow) java.nio.ReadOnlyBufferException
```

Every read-only subclass overrides the mutators with a bare
`throw new ReadOnlyBufferException()` and never consults position/limit, so the
refusal is unconditional and comes first. Views of a read-only buffer stay
read-only:

```
scb.slice().isReadOnly=true      scb.duplicate().isReadOnly=true
hcbR.slice().isReadOnly=true  hasArray=false
hcbR.subSequence(0,2).isReadOnly=true
```

---

## 2. What was there

| site | before | HotSpot |
|---|---|---|
| `hasArray()Z` (L1185 pre-edit) | `cb_read_hb(..).is_some()` — the `&& !isReadOnly` half was **absent** | `(hb != null) && !isReadOnly` |
| `array()[C` (L1712 pre-edit) | ONE branch: no `hb` → `IllegalStateException("CharBuffer.array: no backing array")`; **array-backed read-only handed the `char[]` straight out** | UOE / ROBE / the array |
| `arrayOffset()I` (L1277 pre-edit) | `offset` by name, default `0` — **neither refusal** | UOE / ROBE / `offset` |
| `put(C)` (L1562 pre-edit) | overflow check, then **wrote** | ROBE first, unconditionally |
| `subSequence(II)` (L1483 pre-edit) | wrote a flat `isReadOnly = 0` on the result | the view inherits read-only |

Three separate defect species in one family:

1. **A wrong exception class in a hierarchy nothing catches.**
   `IllegalStateException` is in neither of the two the JDK uses, so
   `catch (UnsupportedOperationException)` and `catch (ReadOnlyBufferException)`
   both missed it and the throw escaped as an unrelated failure.
2. **A wrong ANSWER**: `hasArray()` said `true` for a `HeapCharBufferR`.
3. **A wrong CAPABILITY**, which is worse than either: `array()` on an
   array-backed read-only buffer returned a *mutable alias* to storage the JDK
   guarantees is not writable through that handle, and `put(char)` wrote through
   it directly. A caller that follows the documented `hasArray()` → `array()`
   protocol was steered into it by defect 2.

**This is the drifted-twin shape.** `native-builtins/src/servlet.rs`'s
`register_s2_bytebuffer` has carried all three arms correctly since W7-83 §7.1 —
its `arrayOffset()` comment even quotes the same three-line JDK body. One JVMS
contract, implemented twice, and only one copy was ever repaired.

---

## 3. The fix

One decision function, three call sites, so the ordering has a single
definition:

```rust
enum CbArrayAccess { Accessible, ReadOnly, Absent }

fn cb_array_access(has_hb: bool, read_only: bool) -> CbArrayAccess {
    if !has_hb        { CbArrayAccess::Absent }      // FIRST, per the JDK body
    else if read_only { CbArrayAccess::ReadOnly }
    else              { CbArrayAccess::Accessible }
}
```

* `cb_is_read_only` reads the `isReadOnly` **field** by name — transcribed from
  `servlet.rs::s2_bb_is_read_only` so the twins cannot drift again. §1.2 is why
  reading the field rather than calling the method is correct, and why the
  view-buffer classes that never write it are still served correctly (they have
  no `hb`, so `Absent` answers first).
* `cb_no_backing_array()` uses `RuntimeError::UnsupportedOperationException`
  with an **empty** message, which `types/src/error.rs` maps to the `()V`
  constructor — so `getMessage()` is null, exactly as HotSpot's
  `new UnsupportedOperationException()`.
* `put(char)` refuses before reading position/limit (§1.3).
* `subSequence` propagates the source's read-only bit instead of writing `0`.

Reachability, stated because it bounds the claim: `vm/src/vm/vm_exec.rs`'s
`resolve_dispatch` **step 3** makes real class bytes authoritative
(`if method.code().is_some() { return Bytecode }`), and `java/nio/CharBuffer` is
in neither `force_native_over_real_jdk_bytecode`'s list nor the `check_override`
name chain. So in real-JDK mode these natives answer for exactly the receivers
whose resolved method has **no `Code`** — the `--features synthetic-jdk` build,
and the abstract-stamped `java/nio/CharBuffer` instances CratonVM itself mints
(`servlet.rs::s2_bb_as_char_buffer` allocates under the abstract name; see the
`cb_native_order` doc comment). That population is exactly where a read-only
CharBuffer's backing array was being handed out.

### 3.1 Tests

Three `#[test]`s on `cb_array_access`, not on a receiver. Deliberate:
`MockNativeContext` resolves field NAMES through a slot fallback, so a
receiver-shaped assertion here would be measuring the mock's name-to-slot table
rather than the ordering. The tests pin the truth table, the
`Absent`-before-`ReadOnly` order on its own (the single cell a read-only-first
implementation gets wrong), and that exactly one of the four states hands the
array over.

---

## 4. Swept and CLEARED — what this lane checked and did NOT change

Against the same transcript, in `register_p62_char_buffer`:

| row | CratonVM | oracle | |
|---|---|---|---|
| `CharBuffer.allocate(-1)` | `IllegalArgumentException("capacity < 0: (-1 < 0)")` | `msg=capacity < 0: (-1 < 0)` | message-exact |
| `position(9)` | `IAE("newPosition > limit: (9 > 4)")` | same | message-exact |
| `position(-1)` | `IAE("newPosition < 0: (-1 < 0)")` | same | message-exact |
| `limit(9)` | `IAE("newLimit > capacity: (9 > 4)")` | same | message-exact |
| `get(int)` out of range | `ioobe_no_message()` | `IndexOutOfBoundsException`, null message | class + null message |
| relative `get()` at the limit | `BufferUnderflowException` | same | |
| relative `put()` at the limit | `BufferOverflowException` | same (writable receiver) | and now ROBE first when read-only |
| `subSequence(0, 9)` | `check_from_to_index` → `Range [0, 9) out of bounds for length 4` | identical string | message-exact |

Not registered on `java/nio/CharBuffer` in real-JDK mode, so real bytecode
serves them and they are correct by construction — recorded so the next lane
does not re-derive it: `wrap([C)`, `wrap(CharSequence)` and
`wrap(CharSequence,int,int)` (deliberately unregistered, see the standing
comment at the top of the registrar), `put(int,char)`, `put(String)`,
`compact()`, `asReadOnlyBuffer()`, `duplicate()`, `slice()`, `slice(int,int)`,
`charAt(int)`, `reset()`, `mark()`. The absolute and relative bulk contracts
(`IndexOutOfBounds` for a bad ARRAY range, `BufferUnderflow`/`BufferOverflow`
for a short BUFFER) are likewise real bytecode here; the oracle rows are in the
probe (`cb.get(char[4],0,8)` → `Range [0, 0 + 8) out of bounds for length 4`;
`cb.get(char[8])` → `BufferUnderflowException`; `cb.put(char[8])` →
`BufferOverflowException`) so a future registration has its expected values
already measured.

### 4.1 Known gap NOT fixed here

`get()C` / `get(I)C` in this registrar read the backing array only and raise
`IllegalStateException("no backing array")` when there is none. A real
`StringCharBuffer` holds its text in `str`, not `hb` — but it also carries its
own `Code` for both, so it never reaches these natives. Left alone rather than
grown a `str` arm for a receiver population that does not exist; the
`toString(II)` native two hundred lines above already has that arm if it ever
does.

---

## 5. Predicted effect on `RJdkIntrinsics2 --only=bounds`

`bounds` is 93 checks and `check()` throws on the FIRST failure, so everything
after the failing row is unexecuted. Predicted flips, in fixture order:

| line | row | before | after |
|---|---|---|---|
| 2986 | `CharBuffer.wrap(String).array()` must be `UnsupportedOperationException` | **RED** | **GREEN** |
| 2989 | `.put(0,'x')` must be `ReadOnlyBufferException` | never ran | GREEN (real `StringCharBuffer.put(int,char)` bytecode) |
| 3001 | `allocate(4)` `hasArray() && array().length == 4 && arrayOffset() == 0` | never ran | GREEN — `cb_write_hb` writes `isReadOnly = 0` and `offset = 0` by name, so the writable cell is unchanged |
| 3005 | `put(9,'z')` must be `IndexOutOfBoundsException` | never ran | GREEN |
| 3014 | `allocate(-1)` must be `IllegalArgumentException` | never ran | GREEN (§4, message-exact) |
| 3041 | `wrap(seq, 1, 9)` must be `IndexOutOfBoundsException` | never ran | GREEN (real bytecode) |
| 3051/3062/3071 | `position(9)`, `limit(9)`, `position(-1)` must be `IllegalArgumentException` | never ran | GREEN (§4, message-exact) |
| 3087 | `subSequence(0, 9)` must be `IndexOutOfBoundsException` | never ran | GREEN |
| 3103 | relative `get()` past the limit must be `BufferUnderflowException` | never ran | GREEN |
| 3113 | `reset()` with no mark must be `InvalidMarkException` | never ran | GREEN (real `Buffer.reset()`) |
| 3177 | `asReadOnlyBuffer().putChar(0,'x')` must be `ReadOnlyBufferException` | never ran | GREEN (real `HeapByteBufferR` bytecode) |

The rows between are `String` and `ByteBuffer` and this lane touched neither.

---

## 6. If 2986 is still red after this — the discriminator

The brief quoted the assertion text but not the `got …` tail, and the tail
decides which of two mechanisms produced the wrong answer. **Read it first:**

* **`got java.lang.IllegalStateException`** — this native was the answer. The
  fix lands it, and it also means a registered `java/nio/CharBuffer` native is
  reaching a real `StringCharBuffer` receiver, which `resolve_dispatch` step 3
  says should not happen. Worth a second record on its own.
* **`got java.nio.ReadOnlyBufferException`** — real `CharBuffer.array()`
  bytecode ran and its `getfield hb` answered **non-null** for a receiver whose
  constructor passed `null`. This fix does not touch that; the defect is in
  field resolution for `java/nio/StringCharBuffer` (whose layout is
  `Buffer{mark,position,limit,capacity,address,segment}` +
  `CharBuffer{hb,offset,isReadOnly}` + `StringCharBuffer{str}`), and it is
  invisible to every other row in the fixture because `hasArray()` is
  `(hb != null) && !isReadOnly` and the `isReadOnly` half masks it. That is N2.
* **`got none`** — `array()` returned an array, so `hb` is non-null AND the
  `isReadOnly` field is false while `isReadOnly()` answers true (the method is a
  hard-coded `return true` on `StringCharBuffer`, so the two can disagree). Same
  root as above, opposite half.

---

## 7. NOMINATIONS

**N1 — `native-io/src/lib.rs`: the same three natives, read-only-blind.**
`native_bb_array` (L9703), `native_bb_has_array` (L9717),
`native_bb_array_offset` (L9730) and `native_tb_array` (L16018) classify storage
as Heap-or-Direct and **never consult `isReadOnly` at all**: a read-only heap
buffer answers `hasArray() == true`, and `array()` hands out its backing array,
where §1.1 measures `false` / `ReadOnlyBufferException`. `native_tb_array`
additionally returns a **null array** for a storage-less receiver where HotSpot
throws `UnsupportedOperationException` — a null that flows to the caller's
`arraylength` as a NullPointerException at some unrelated site. These are
registered on `java/nio/{Byte,Char,Int,Long,Float,Double,Short}Buffer` by
`register_nio_natives`, which runs only in a `--features synthetic-jdk` build
with `drop_real_layout_synthetic` unset. The oracle rows for all seven element
types are in `probes/BufferAccessibleArrayProbe`. Not this lane's file
(`lib.rs`).

**N2 — `getfield hb` on a real `java.nio.StringCharBuffer`.** Conditional on §6
resolving to `ReadOnlyBufferException` or `none`. Cheapest probe: a fixture that
prints `CharBuffer.wrap("abcd").hasArray()`, `.isReadOnly()` **and**
`.array()`'s exception class together — the first two agree with HotSpot whether
or not `hb` is null, so only the third discriminates, and no existing vector
asks it.

**N3 — `servlet.rs`'s ByteBuffer UOE carries a detail message.**
`register_s2_bytebuffer`'s `array()`/`arrayOffset()` raise
`UnsupportedOperationException { message: "direct buffer has no backing array" }`
where HotSpot's `new UnsupportedOperationException()` has a **null**
`getMessage()`. The class is right, so no `catch` sees a difference and no
current row fails; a message-exact differential would. The CharBuffer side now
uses the empty-message spelling that `types/src/error.rs` maps to the `()V`
constructor, so the fix is a two-line transcription in the other direction.

**N4 — `RJdkIntrinsics2` has no row for the READ-ONLY ARRAY-BACKED cell.**
`bounds` asserts the `StringCharBuffer` cell (`array()` → UOE) and the writable
cell (`allocate(4)`), and never the third: `CharBuffer.allocate(4)
.asReadOnlyBuffer()` must answer `hasArray() == false`, `array()` →
`ReadOnlyBufferException`, `arrayOffset()` → `ReadOnlyBufferException`. That is
the cell where the pre-fix code handed out a mutable alias, and the fixture
would have passed regardless. Three checks, all measured in §1.1. The
`ByteBufferAsIntBufferB` cell of §1.2 (writable, array-less, UOE) is a fourth,
and is what proves the split is not "read-only vs not".
