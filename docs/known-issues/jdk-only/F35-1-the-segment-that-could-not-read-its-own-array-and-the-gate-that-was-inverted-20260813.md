# F35-1 — the segment that could not read its own array, the slice that rebuilt the crash one level up, and a gate that was inverted per mode

**2026-08-13, lane F35.** Lands F27-1's three nominations and W7-89 §7.2.
Patches exactly one file, the only one this lane owns:

* `native-builtins/src/panama.rs` (**verified path**; CRLF, 7369/7369 CR/LF)

The patch is in the working tree, uncommitted.

**This lane may not build or run CratonVM, and did not.** Every JDK fact below
is `javap` / `java` on this host (Microsoft build **25.0.3+9-LTS**) or the JDK
source at `C:\craton\jdk25src`, quoted at the point it is used. Every claim
about CratonVM's behaviour — before and after — is **PREDICTED** from source.
The file was parse-checked (`rustfmt --edition 2021 --emit stdout` on a scratch
copy, exit 0), which rules out syntax errors and nothing else; it was not
type-checked and the fifteen new tests were not run.

**Line endings.** `panama.rs` is CRLF and stayed CRLF: **7369 CR / 7369 LF**,
checked with `tr -cd '\r' < f | wc -c` against `tr -cd '\n' < f | wc -c`, as
F16-1 §0 instructs. `grep -c $'\r'` was not used; it cannot go red in this
shell. Two edits were made with Python reading and writing **bytes** (`'rb'` /
`'wb'`, no newline translation) rather than `sed`/`awk`, which run in text mode
here and strip CRs. This record is LF, matching every other file in this
directory.

---

## 0. Verdict

| claim | verdict |
|---|---|
| **the method note this lane was briefed with — "panama's registrations do not exist in real-JDK or `--jdk-only` mode at all"** | **HALF WRONG, AND IT IS THE HALF THAT DECIDES WHETHER ANY OF THIS SHIPS.** `register_pe_panama` is synthetic-only, but **four of its eleven children have a SECOND call site inside the always-on real-JDK registrar**, and one of them is `register_pe_memory_segment` — which owns `ofArray`, `get`, `set`, `getAtIndex`, `setAtIndex`, `asSlice`, `address`, `isNative`, `copy`, `fill`, `ofAddress`. The downcall path reaches `panama::pe_downcall_invoke` too, from `foreign_ffm.rs`. **Tasks 1, 2 (in part), 3 and 4 all land in `--jdk-only`** (§1) |
| F27 NOM-1: the clamp comment is false | **CONFIRMED, COMMENT CORRECTED, BOUND KEPT** — and the paragraph ABOVE the one F27 quoted was stale too and is corrected with it (§2) |
| F27 NOM-2: three `kind < 10` tests admit `LAYOUT_UNKNOWN` | **CONFIRMED AND CLOSED**, plus a fourth site F27's nomination did not list, and the `-1` void sentinel is now `UPCALL_RETURN_VOID` (§3) |
| F27 NOM-3: heap segments cannot be read or written | **CONFIRMED AND IMPLEMENTED.** They now read and write the backing Java array, with the oracle's alignment rule, the oracle's exception classes, and read-only enforcement (§4) |
| F27 NOM-3: `ofArray` covers four of seven | **CONFIRMED AND CLOSED** — and the three new arms **ALIAS** where the existing four **COPY**, deliberately, because the copy is a wrong capability in both directions and the oracle refuses what it enables (§5) |
| **F27's fix did not close W7-89 §7.1; it moved it** | **NEW, AND IT IS A LIVE SIGSEGV TODAY.** `asSlice(long,long)` on a heap segment computed `segment_address(this) + offset` — `0 + offset` since F27 — and stamped that into slot 0 of a synthetic. `ofArray(new byte[16]).asSlice(3, 4).get(...)` dereferences the literal address **3**. At offset 0 it stays 0 and refuses, which is exactly why the un-sliced repro looked fixed (§6) |
| **`MemorySegment.address()` was answering the wrong one of two questions** | **NEW.** `segment_address` is "the address to dereference" and must stay 0 for a heap carrier; `MemorySegment.address()` is the JDK's accessor and is **3** for `ofArray(new byte[32]).asSlice(3)`. One function answered both (§6.2) |
| W7-89 §7.1's attribution | **F27's re-attribution is RIGHT about the mechanism and its evidence is weaker than it says.** For `new byte[16]` the `length` field is 16 **and the `offset` field is also 16**, so `0x10` does not discriminate between them. The code path does (§7) |
| W7-89 §7.2: `MemorySegment.set` refused without `--enable-native-access` | **CONFIRMED AND FIXED.** Only the three `reinterpret` overloads are `@Restricted` on JDK 25. The accessors are not (§8) |
| **the native-access gate is INVERTED between the two modes** | **NEW.** In `--jdk-only` the one method the JDK really does restrict — `reinterpret` — runs through `foreign_ffm.rs`'s **ungated** registration, while `get`/`set`, which the JDK does not restrict, were refused. NOMINATED (§8.2) |
| the erased `getAtIndex`/`setAtIndex` | **NEW DEFECT, and it is a second copy of one rule.** They open-coded the stride as `get_field(layout, 1)` matched against `Value::Int` — the **deleted** three-slot encoding — so every call took the `_ => 1` default and the stride was **one byte** (§9) |
| the zero-size refusal class | **NEW, small.** `IllegalStateException` where the oracle throws `IndexOutOfBoundsException` — contradicting the "Bounds, not state" paragraph in the same function (§10) |

---

## 1. Which mode each fix reaches — the fact that decides whether this ships

The briefing said `register_pe_panama`'s only call site is inside
`#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides`, and that
therefore "panama's registrations do not exist in real-JDK or `--jdk-only` mode
at all." The first clause is true (F16-1 §1.2 measured it). The conclusion does
not follow, because `register_pe_panama` is not the only caller of its own
children:

```
$ grep -n "crate::panama::register_" native-builtins/src/lib.rs
  10037:    crate::panama::register_pe_raw_native_libraries(registry);
  10043:    crate::panama::register_pe_symbol_lookup(registry);
  10047:    crate::panama::register_pe_linker_options(registry);
  10057:    crate::panama::register_pe_memory_segment(registry);
$ grep -n "^pub fn register_essential_natives_with_shims" native-builtins/src/lib.rs
  7108:pub fn register_essential_natives_with_shims(
```

`:10057` is inside `register_essential_natives_with_shims` — the always-on
real-JDK path, no `#[cfg]` — and it runs **after**
`crate::phases_late::register_p67_foreign_memory(registry)` at `:10053`, so on a
shared key panama wins there too. The comment at the call site says so in as
many words: *"Keep the full MemorySegment bridge in the always-on real-JDK
registry."*

Measured registrar boundaries in `panama.rs` after this patch:

| registrar | span | reached by |
|---|---|---|
| `register_pe_memory_segment` | `:735`–`:1566` | **real-JDK / `--jdk-only` AND synthetic** |
| `register_pe_linker` (`downcall`, `upcallHandle`) | `:3111`–… | synthetic only |
| `register_pe2_string_marshaling` (holds `reinterpret`, `:4717`) | `:4655`–… | synthetic only |

and, independently, `foreign_ffm.rs:4029/4035/4041` register
`crate::panama::pe_downcall_invoke` for three descriptors from
`register_p67_foreign_memory` (`:2352`), which is on the real-JDK path — so the
downcall body in this file runs in `--jdk-only` even though panama's own
`Linker` registrar does not.

**Per-fix reach:**

| fix | reaches |
|---|---|
| §2 clamp comment (Task 1) | comment only; the code it documents is **`--jdk-only` + synthetic** (via `foreign_ffm`'s `pe_downcall_invoke` registrations) |
| §3 `layout_kind_is_value` in the downcall return unmarshal | **`--jdk-only` + synthetic** |
| §3 `downcall_layout_carrier_descriptor` | **`--jdk-only` + synthetic** (`pe_downcall_type` is registered at `foreign_ffm.rs:4047`) |
| §3 `UPCALL_RETURN_VOID` rename | **synthetic only** — `upcallHandle` has no real-JDK registration. Hygiene, and stated as such |
| §4 heap-segment get/set | **`--jdk-only` + synthetic**, and `--jdk-only` is where the carrier exists at all |
| §5 `ofArray([B/[S/[C)` | **`--jdk-only` + synthetic** |
| §6 `asSlice` heap arm | **`--jdk-only` + synthetic** |
| §6.2 `address()` | **`--jdk-only` + synthetic** |
| §8 accessor gate (Task 4) | **`--jdk-only` + synthetic** |
| §9 erased `getAtIndex`/`setAtIndex` | **`--jdk-only` + synthetic**; reachable by reflection/`MethodHandle`, not by a `javac` call site |
| §10 zero-size exception class | **`--jdk-only` + synthetic** |

One fix is synthetic-only. Everything substantive is not.

---

## 2. Task 1 — the clamp comment (F27 NOM-1)

`native-builtins/src/panama.rs`, in `pe_downcall_invoke`'s aggregate-return arm.
The bound **stays**; F27's reasoning is right and is now recorded at the site: a
no-op bound guarding an `unsafe copy_nonoverlapping` whose two sizes come from
two different files is the local proof that the copy is in bounds, and deleting
it makes an `unsafe` block's safety argument non-local.

F27's exact NEW text was applied verbatim. **One thing F27's nomination
missed**: the paragraph immediately above the quoted block was stale in the same
way — it said *"while that function still answers 8 for a group layout"*, which
stopped being true when F27 landed. Correcting only the quoted half would have
left the site self-contradicting. It now reads:

```rust
                // `ret_slot` is sized by `panama_libffi::alloc_return_slot`
                // and `total` by `foreign_ffm::p67_layout_size_of` — two size
                // functions in two files, with one `unsafe
                // copy_nonoverlapping` between them. …
```

No behavioural change. `grep -c "Clamping keeps this side correct"` is now
**0**, and `grep -c "IT STAYS ANYWAY"` is **1**.

---

## 3. Task 2 — `LAYOUT_UNKNOWN`, and the sentinel that forced it to be `-2`

### 3.1 The predicate

`layout_kind_is_value(kind) -> bool` is `(0..10).contains(&kind)`, with the
reason at the definition. The kinds are `0..=8` for value layouts and `10..=13`
for the group family, so `kind < 10` **looks** like the same test and is not: it
is true for every negative kind, and `LAYOUT_UNKNOWN` is `-2`.

### 3.2 The sites, and a fourth F27's nomination did not list

| site | old default for `LAYOUT_UNKNOWN` | now |
|---|---|---|
| `pe_downcall_invoke`'s return unmarshal, `} else if kind < 10 {` | `unmarshal_return_primitive`'s own default arm | explicit `LAYOUT_UNKNOWN` refusal, then `layout_kind_is_value(kind)` |
| `pe_segment_get_impl` | `layout_byte_size` = **1**, then `_ => Value::Int(0)` — a believable zero | refused by name at the top of the function |
| `pe_segment_set_impl` | `layout_byte_size` = 1, then `_ => {}` — a **silent no-op write** | refused by name at the top |
| **`downcall_layout_carrier_descriptor` — NOT in F27's list as a code change** | `_ => "Ljava/lang/foreign/MemorySegment;"` | takes the **layout**, returns `Result`, refuses by name |

F27's NOM-2 named `downcall_layout_carrier_descriptor` in prose but asked only
for an "explicit `plf::LAYOUT_UNKNOWN => …refuse…` arm". It cannot have one
usefully as written, because its parameter is a bare `i32` and a refusal that
cannot name the carrier is not actionable. Its signature changed to
`(ctx, layout) -> Result<&'static str, MethodCallFailed>`; both call sites are
already in a `Result` context. **This is the one of the four whose wrongness
would not have been local**: it is the descriptor the downcall's `MethodType` is
built from, so a wrong letter puts every later argument in the wrong slot kind.

None of the four was reachable — `alloc_return_slot` and `layout_to_ffi_type`
both refuse an unknown carrier earlier, on every path. That is a property of
today's callers, not of the code.

### 3.3 The `-1` void sentinel

`panama.rs`'s upcall-stub builder used a bare `-1` for "void return", and F27's
record says that collision is why `LAYOUT_UNKNOWN` had to be `-2`. It is now
`UPCALL_RETURN_VOID`, a named constant whose doc says it is **not a layout
kind** and that every real kind is `>= 0`. Two sites in this file
(`pe_upcall_stub`'s `None => …` and `upcall_dispatch`'s marshalling match).

**Reach: synthetic only.** `Linker.upcallHandle` has no real-JDK registration —
`register_pe_linker`'s only caller is `register_pe_panama`. Stating that is the
point: this one is hygiene against a reorder, not a shipping fix.

**NOMINATED, not done:** `native-api/src/ffi.rs` writes the same bare `-1` into
`UpcallEntry.return_kind` in three of its own tests (`:1199`, `:1206`, `:1234`).
That is the struct the value travels in, so the constant arguably belongs there.
Not this lane's file.

---

## 4. Task 3 — heap segments now read and write their backing array

### 4.1 The two carriers

```rust
struct HeapSegmentView { base, start, size, read_only, elem_width, elem_type }
```

resolved by `heap_segment_view`, which recognises exactly two shapes:

* **H1** — a real JDK `jdk.internal.foreign.HeapMemorySegmentImpl$Of*`.
  Measured field order (`javap -p`, 25.0.3+9-LTS): `AbstractMemorySegmentImpl{length, readOnly, scope}` then `HeapMemorySegmentImpl{offset, base}`.
* **H2** — a CratonVM alias carrier, `[6] = the array`, `[7] = byte start`,
  slot 0 deliberately **0**. Minted by §5's `ofArray` arms and by §6's `asSlice`.

`H2` does **not** reuse slots 2/4/5. `segment_address` answers `[0] + [5]` for
any carrier with six or more fields, so putting the byte start in slot 5 would
hand every raw-pointer consumer in the tree a small integer to dereference —
which is the exact defect F27 closed one level down.

### 4.2 The bias, measured

`HeapMemorySegmentImpl.offset` is an `Unsafe`-style offset with
`arrayBaseOffset` baked in. Measured, with
`--add-opens java.base/jdk.internal.foreign=ALL-UNNAMED`:

```
$ java F35Probe                                            # 25.0.3+9-LTS
ofArray(byte[32])    class=…HeapMemorySegmentImpl$OfByte   byteSize=32 address()=0 offset=16 length=32 base=[B
ofArray(short[8])    class=…$OfShort                       byteSize=16 address()=0 offset=16 length=16 base=[S
ofArray(char[8])     class=…$OfChar                        byteSize=16 address()=0 offset=16 length=16 base=[C
ofArray(int[8])      class=…$OfInt                         byteSize=32 address()=0 offset=16 length=32 base=[I
ofArray(long[8])     class=…$OfLong                        byteSize=64 address()=0 offset=16 length=64 base=[J
ofArray(float[8])    class=…$OfFloat                       byteSize=32 address()=0 offset=16 length=32 base=[F
ofArray(double[8])   class=…$OfDouble                      byteSize=64 address()=0 offset=16 length=64 base=[D
ofBuffer(ByteBuffer.allocate(16))   class=…$OfByte         byteSize=16 address()=0 offset=16 length=16 base=[B
ofBuffer(ByteBuffer.allocateDirect(16)) class=NativeMemorySegmentImpl byteSize=16 address()=1935226488208 base=null
ofArray(byte[32]).asSlice(3)        class=…$OfByte         byteSize=29 address()=3  offset=19 length=29 base=[B
```

**16 for every primitive array type**, and CratonVM answers 16 for every array
type too — `unsafe_natives_ext::native_unsafe_array_base_offset` is a constant
`Ok(Some(Value::Int(16)))`. So one constant, `HEAP_ARRAY_BASE_OFFSET`, serves
both carriers, and `start = offset - 16` is the JDK's own `address()`.

Note the `ofBuffer` row: **`MemorySegment.ofBuffer(ByteBuffer.allocate(16))` is
a heap segment too.** `ofArray` is not the only door, which is why §5's three
new registrations are not on their own a fix.

### 4.3 Byte addressing and endianness, measured

```
$ java F35Probe / F35Probe2                                # 25.0.3+9-LTS
byte[8]  set(JAVA_INT_UNALIGNED,0,0x01020304)  -> [4, 3, 2, 1, 0, 0, 0, 0]
short[4] set(JAVA_LONG_UNALIGNED,0,0x0102030405060708)
                                               -> [0x0708, 0x0506, 0x0304, 0x0102]
char[4]  set(JAVA_INT_UNALIGNED,0,0x00420041)  -> ['A', 'B', ' ', ' ']
int[4]   set(JAVA_BYTE,0,0x7f)                 -> iarr[0] == 127
int[4]   set(JAVA_INT,4,0x11223344)            -> iarr[1] == 0x11223344
int[4]   get(JAVA_BYTE,3)                      -> 0
float[2] set(JAVA_FLOAT,0,1.5f); get(JAVA_INT,0) -> 0x3fc00000  (== floatToRawIntBits)
double[2] set(JAVA_DOUBLE,8,2.25)              -> [0.0, 2.25]
```

So byte offset `k` is element `k / width`, byte `k % width`, little-endian
through the element **and** through the array; a one-byte write must not disturb
the other three bytes of its element; and a `float[]` is a bit view, not a
value view. `heap_segment_read`/`heap_segment_write` implement exactly that,
read-modify-write per byte because an access is at most 8 bytes wide and may
start and end mid-element.

`heap_element_width` refuses `Reference` (no byte view of an object array) and
refuses **`Boolean`** — measured, `MemorySegment` has **no** `ofArray(boolean[])`
overload:

```
$ java F35Probe2
ofArray(boolean[]) -> java.lang.NoSuchMethodException: java.lang.foreign.MemorySegment.ofArray([Z)
```

so inventing a byte semantics for one would be a defaulting reader in a family
whose every defect has been a defaulting reader.

### 4.4 Alignment — enforced on the heap path, with BOTH halves

```
$ java F35Probe                                            # 25.0.3+9-LTS
byte[32] get(JAVA_INT,0)            -> IllegalArgumentException: Target offset 0 is
                                       incompatible with alignment constraint 4 (of i4) …
byte[32] get(JAVA_INT_UNALIGNED,0)  -> OK
byte[32] get(JAVA_LONG_UNALIGNED,0) -> OK
byte[32] get(ADDRESS,0)             -> IllegalArgumentException (constraint 8, of a8)
byte[32] get(ADDRESS align 1,0)     -> OK
int[8]   get(JAVA_INT,0)            -> OK
int[8]   get(JAVA_INT,1)            -> IllegalArgumentException (offset 1)
int[8]   get(JAVA_LONG,0)           -> IllegalArgumentException (constraint 8)
int[8]   get(JAVA_LONG_UNALIGNED,0) -> OK
long[8]  get(JAVA_LONG,0)           -> OK
maxByteAlignment: byte[]=1 int[]=4 long[]=8
```

The rule has two halves and **both are load-bearing**: a `byte[]` segment's
address is 0, so `(address + offset) % 4 == 0` alone would ADMIT
`get(JAVA_INT, 0)` on one, where the oracle refuses it; and
`maxByteAlignment` alone would admit `int[8] get(JAVA_INT, 1)`. The
implementation checks `align <= max_align && (start + offset) % align == 0`,
with `max_align` the element type's alignment, capped by the low bit of `start`
when it is non-zero — which is what makes `asSlice(3)` (address 3, lowest set
bit 1) reject every aligned layout, as the oracle does.

**Deliberately NOT transplanted to the raw-address path.** A native segment's
real `maxByteAlignment` is not knowable from the carrier — the oracle answers
**32** for a malloc'd one — so the same rule cannot be applied there, and adding
a half-rule to a path that works today would be a regression. NOMINATED (§11).
Enforcing it on the heap path has **zero** regression risk in the other
direction: no heap access could succeed at all before this patch.

### 4.5 The exception classes are the oracle's

```
$ java F35Probe / F35Probe2
byte[8] get(JAVA_INT_UNALIGNED,6)          -> IndexOutOfBoundsException
byte[8] get(JAVA_BYTE,-1)                  -> IndexOutOfBoundsException
byte[0] get(JAVA_BYTE,0)                   -> IndexOutOfBoundsException
byte[8].asReadOnly().set(JAVA_BYTE,0,1)    -> IllegalArgumentException: Attempt to write a read-only segment
byte[8].asReadOnly().get(JAVA_BYTE,0)      -> OK
closed-arena native seg get(JAVA_BYTE,0)   -> IllegalStateException: Already closed
```

Liveness is still checked first and still raises `IllegalStateException`;
everything else is `IndexOutOfBoundsException` or, for the read-only and
alignment violations, `IllegalArgumentException`.

---

## 5. `ofArray` covers seven now — and three of them ALIAS

```
$ javap -p java.lang.foreign.MemorySegment | grep ofArray   # 25.0.3+9-LTS
  public static MemorySegment ofArray(byte[]);
  public static MemorySegment ofArray(char[]);
  public static MemorySegment ofArray(short[]);
  public static MemorySegment ofArray(int[]);
  public static MemorySegment ofArray(float[]);
  public static MemorySegment ofArray(long[]);
  public static MemorySegment ofArray(double[]);
```

Seven, no `boolean[]`. This file registered four. `ofArray` **is** on
`native_override.rs`'s force-route name list, but a forced name with no
registration for that descriptor falls back to real bytecode, so in `--jdk-only`
the three missing arms produced a real `HeapMemorySegmentImpl$OfByte/OfShort/OfChar`
— and that is the W7-89 §7.1 crash. In synthetic-JDK mode there is no bytecode
to fall back to at all.

### 5.1 Why the new three do not copy

The existing four allocate an off-heap mirror and copy the array into it,
because CratonVM cannot hand a moving Java array to native code; those carriers
exist to be passed to downcalls, and `sync_heap_backed_segment` copies back at
the boundary. **The oracle does not permit that at all, and does require the
aliasing the mirror cannot give:**

```
$ java F35Probe2                                           # 25.0.3+9-LTS
strlen(MemorySegment.ofArray("hi\0".getBytes()))
  -> IllegalArgumentException: Heap segment not allowed:
     MemorySegment{ kind: heap, heapBase: [B@…, address: 0x0, byteSize: 3 }
byte[] a = new byte[8];
MemorySegment.ofArray(a).set(JAVA_INT_UNALIGNED, 0, 0x01020304);   // a == [4,3,2,1,0,0,0,0]
```

So the mirror is a wrong capability in **both** directions: it enables a
downcall the JDK refuses, and it does not alias reads. Propagating it to three
more descriptors, when the alias carrier costs nothing and needs no native
allocation, would have been propagating a known defect three times.

**This is the one asymmetry in the registrar and it is stated at the
registration site**, not left to be discovered. Unifying the other four is
NOMINATED (§11), not done: they are the shape Elasticsearch's bulk-vector
downcalls depend on, and converting them is a separate blast radius.

### 5.2 The `ADDRESS` write, which was the quiet half

`set(ADDRESS, off, seg)` resolved its value through `segment_address(target)`,
which since F27 answers **0** for a heap segment — a legitimate C null that no
downstream consumer can tell apart from a real one. The oracle refuses:

```
$ java F35Probe
MemorySegment.ofArray(new byte[16])
    .set(ADDRESS.withByteAlignment(1), 0, MemorySegment.ofArray(new byte[4]))
  -> IllegalArgumentException: Heap segment not allowed: MemorySegment{ kind: heap, … }
```

the same message the downcall refuses with. It now refuses by name, with a
control in the tests that a native segment is still a legal `ADDRESS` value.

---

## 6. F27's fix did not close W7-89 §7.1 — `asSlice` rebuilt it one level up

**This is the finding that matters most in this record.**

`asSlice(long, long)` is registered by `register_pe_memory_segment`, i.e. it is
live in `--jdk-only`, and it did:

```rust
let base_ptr = crate::panama_libffi::segment_address(ctx, this);
let slice_ptr = base_ptr.checked_add(offset)…;
…
ctx.set_field(slice, 0, Value::Long(slice_ptr));      // slot 0 is the ADDRESS
```

For a heap receiver `segment_address` answers **0** after F27, so `slice_ptr` is
the slice's **offset**, stamped into the address slot of a six-field synthetic —
reconstructing, one level up, exactly the defect F27 closed: a small integer
that is not an address, sitting where every consumer reads an address.

```
MemorySegment.ofArray(new byte[16]).asSlice(3, 4).get(JAVA_BYTE, 0)
  → dereferences the literal address 3
```

At offset 0 the product is 0 and `pe_segment_access_addr` refuses, which is
**why the un-sliced W7-89 §7.1 repro looks fixed while the sliced one still
dies.** A fix that turns a wild pointer into 0 has to be followed to every
consumer of that 0; this one was not.

`asSlice` now mints an H2 heap carrier when its receiver is a heap segment.
Oracle rows it matches:

```
$ java F35Probe2
ofArray(new byte[16]).asSlice(3, 4):  byteSize=4  address()=3  isNative()=false
byte[] src = new byte[16];
MemorySegment.ofArray(src).asSlice(3, 4).set(JAVA_INT_UNALIGNED, 0, 0x01020304);
// src == [0, 0, 0, 4, 3, 2, 1, 0, 0, …]
```

The closure was extracted into a named `pe_segment_as_slice` so the heap arm is
reachable from a unit test at all. An anonymous closure inside a registrar can
only be exercised by standing a whole `NativeMethodRegistry` up, which is part
of why the defect had no test in either direction.

### 6.1 `isNative()`

The old discriminator was "is there a Java array in `SEG_BACKING_ARRAY_FIELD`",
which is `false` for a real `HeapMemorySegmentImpl` and for an H2 carrier — so
`isNative()` answered **true** for both. Oracle: `ofArray(new byte[32]).asSlice(3, 4).isNative()`
is **false**. It now also consults `is_real_heap_segment` and the heap view.

### 6.2 `address()` — two questions, one function

`panama_libffi::segment_address` means "the machine address to dereference", and
0 is the poison every consumer already refuses on. `MemorySegment.address()` is
the JDK's accessor and has a defined answer that is **not** always 0:

```
ofArray(new byte[32]).address()            == 0   (offset field 16)
ofArray(new byte[32]).asSlice(3).address()  == 3   (offset field 19)
```

The registration in this file forwarded to the former. Answering 0 for the slice
is the same class of wrong answer as answering the length was — a plausible
number from a reader that could not decode the carrier — it is just quieter,
because 0 is also the right answer for the un-sliced case every existing test
uses. `address()` now answers `HeapSegmentView::start` for a heap carrier and
leaves `segment_address` alone.

---

## 7. W7-89 §7.1 — confirming F27, and weakening one line of its evidence

F27 re-attributed §7.1's `EXCEPTION_ACCESS_VIOLATION … read at address 0x10`
from "a `Buffer.address` read as a pointer" to `segment_address` answering the
heap segment's `length`, on the grounds that `0x10 == 16 == new byte[16].length`.

**The mechanism is confirmed** — a real `HeapMemorySegmentImpl$OfByte` has five
fields, fewer than the six that select the synthetic `[0]+[5]` arm, so
`segment_address` fell through to `get_field(seg, 0)`, and slot 0 is `length`.

**The numeric argument does not discriminate.** Measured: for
`ofArray(new byte[16])` the `length` field is 16 **and the `offset` field is
also 16** (the array base offset). Both are `0x10`. A repro with `new byte[32]`
would have separated them — `length=32, offset=16` — and §7.1's repro did not.
The proof is the control flow, not the constant.

**Both records agree on the thing that matters**: `0x10` is not a
`Buffer.address` read, the tagged-arena-handle family (`0x4000_0010_…`, bit 62
set) is a different defect with a different signature, and W7-89 §12.4 builds on
the wrong attribution. F27's NOM-4 asking that §12.4 be narrowed stands, and
this lane adds: **§7.1 must not be marked CLOSED yet**, because §6 above shows
the same repro one `asSlice` away was still fatal until this patch.

---

## 8. Task 4 — W7-89 §7.2: the accessors are not `@Restricted`

```
$ grep -n "@Restricted" jdk25src/java.base/java/lang/foreign/MemorySegment.java
  754:    @Restricted    MemorySegment reinterpret(long newSize);
  810:    @Restricted    MemorySegment reinterpret(Arena, Consumer<MemorySegment>);
  869:    @Restricted    MemorySegment reinterpret(long, Arena, Consumer<MemorySegment>);
```

Three methods, all `reinterpret`. `get`, `set`, `getAtIndex`, `setAtIndex`,
`copy`, `fill`, `ofArray`, `asSlice` carry no annotation, and HotSpot 25 runs
every one with no flag. CratonVM raised
`IllegalCallerException: Native access is not enabled for this module
(MemorySegment.set denied)` — which is why every command in W7-89 had to pass
`--enable-native-access=ALL-UNNAMED` to **both** VMs to keep the flag a constant
of the comparison rather than a variable of it. A harness workaround for a VM
defect is the shape `[gap masks bug]` warns about.

`require_segment_access` is a sibling of `require_native_access` that keeps the
host-policy check and the `RawMemory` capability row — so the audit still names
every accessor — and drops only the flag-absent refusal, the half with no
counterpart in the JDK. The six accessor sites moved to it; `downcall`,
`upcallHandle`, `libraryLookup`, `ofAddress` and `reinterpret` keep the strict
gate.

The JDK's safety argument is upstream of the accessor: a segment you can reach
without a restricted call is one whose bounds the runtime knows. Turning an
arbitrary `long` into an addressable segment needs `ofAddress` — measured,
`MemorySegment.ofAddress(0x1000).get(JAVA_BYTE, 0)` is
`IndexOutOfBoundsException` because the segment is **zero-length** — and then
`reinterpret`. Both keep the strict gate here.

### 8.1 A heap segment has no raw memory in it at all

Before §4 that distinction did not matter, because no heap access could succeed
anyway. It does now: a workload that only ever touches `ofArray` segments was
being refused for a raw-memory reason that does not apply to it.

### 8.2 THE GATE IS INVERTED PER MODE — NOMINATED

`panama.rs:4717` registers a `reinterpret` that **is** gated on
`native_access_enabled()`, and it lives in `register_pe2_string_marshaling`,
whose only caller is `register_pe_panama` — **synthetic only**.
`foreign_ffm.rs:3860` registers `MemorySegment.reinterpret(J)` with **no gate at
all**, from `register_p67_foreign_memory`, which is on the real-JDK path.

So in `--jdk-only`, before this patch:

| method | JDK 25 | CratonVM `--jdk-only` |
|---|---|---|
| `reinterpret(long)` | **`@Restricted`** | **ungated** |
| `get` / `set` / `copy` / `fill` | not restricted | **refused without the flag** |

Exactly backwards, in both directions, and only in the shipping mode. This patch
fixes the half in this lane's file. The other half is NOM F35-2 (§11).

---

## 9. The erased `getAtIndex`/`setAtIndex` — one rule, two implementations, adjacent

The erased `(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;` registrations
open-coded the stride:

```rust
let elem_size = match ctx.get_field(layout, 1) {
    Value::Int(n) => n as i64,
    _ => 1,
};
```

That is the **deleted** three-slot encoding, in which slot 1 was
`Int(byteSize)`. Since F16 consolidated on the JDK-true four-slot carrier, slot
1 is `Long(byteAlignment)`, so:

* the `Value::Int` arm cannot match anything this VM or the real JDK mints —
  every call took `_ => 1` and the stride was **one byte**;
  `getAtIndex(JAVA_INT, 2)` read offset **2**, not 8;
* and had it matched, it would have been the **alignment** — 1 for
  `JAVA_INT_UNALIGNED` (measured: `byteSize=4 byteAlignment=1 toString=1%i4`),
  8 for `JAVA_LONG` — a different wrong number per layout.

The covariant registrations a few lines below already routed to
`pe_segment_get_at_index`, which derives the stride from
`ffi::layout_byte_size(read_layout_kind(...))` and is correct. Both spellings
now point at the same function; the duplicate is deleted. Real bytecode emits
the covariant descriptors, so the erased pair is reachable by reflection and by
`MethodHandle`, not by a `javac` call site.

---

## 10. The zero-size refusal class

`pe_segment_access_addr` raised `IllegalStateException` for `size <= 0`, in a
function whose very next paragraph argues that bounds violations must be
`IndexOutOfBoundsException` because "a caller writing
`catch (IndexOutOfBoundsException e)` … did not catch ours". Measured, the
oracle throws `IndexOutOfBoundsException` for a zero-size **native** segment as
well as a heap one:

```
MemorySegment.NULL.get(JAVA_BYTE, 0)          -> IndexOutOfBoundsException
MemorySegment.ofAddress(0x1000).get(JAVA_BYTE, 0) -> IndexOutOfBoundsException
MemorySegment.ofArray(new byte[0]).get(JAVA_BYTE, 0) -> IndexOutOfBoundsException
```

The special case is folded into the bounds check, which **admits nothing new**:
`size == 0` cannot pass `offset + width <= size` for any `width >= 1`. Only the
label changes, to the class a caller can catch. A closed scope is still
`IllegalStateException: Already closed`, raised before it.

---

## 11. NOMINATIONS

### NOM F35-1 — `native-builtins/src/phases_late/foreign_ffm.rs`: `reinterpret` is ungated on the shipping path

Not this lane's file, and the other half of §8.2. `foreign_ffm.rs:3860`
registers `MemorySegment.reinterpret(J)` from `register_p67_foreign_memory`
(real-JDK path) with no native-access check, while `panama.rs`'s gated copy is
synthetic-only and therefore dead in `--jdk-only`. `reinterpret` is one of the
three `@Restricted` methods on `MemorySegment` and is the second half of the
arbitrary-memory primitive (`ofAddress` gives you a zero-length segment; only
`reinterpret` gives it a size).

OLD — anchor on the text, not the line number:

```rust
    r.register(
        "java/lang/foreign/MemorySegment",
        "reinterpret",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = match args.get(1) {
                Some(Value::Long(size)) => *size,
                _ => 0,
            };
```

NEW:

```rust
    r.register(
        "java/lang/foreign/MemorySegment",
        "reinterpret",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            // `reinterpret` is one of exactly THREE `@Restricted` methods on
            // `MemorySegment` (grep the JDK 25 source: lines 754, 810, 869 —
            // all three overloads of this method, and nothing else). It is the
            // second half of the arbitrary-memory primitive: `ofAddress` hands
            // back a ZERO-LENGTH segment that cannot be dereferenced, and only
            // this call gives it a size.
            //
            // This registration had no gate, and `panama.rs`'s gated copy of
            // the same triple is registered by `register_pe2_string_marshaling`
            // — whose only caller is the `#[cfg(feature = "synthetic-jdk")]`
            // `register_pe_panama`. So in `--jdk-only` the ONE method the JDK
            // really does restrict was ungated, while `get`/`set`, which it
            // does not restrict, were refused (F35-1 §8.2).
            if !crate::panama::native_access_enabled() {
                return Err(RuntimeError::IllegalCallerException {
                    message: "Native access is not enabled for this module \
                              (MemorySegment.reinterpret denied)"
                        .into(),
                }
                .into());
            }
            let this = obj_arg(args, 0)?;
            let size = match args.get(1) {
                Some(Value::Long(size)) => *size,
                _ => 0,
            };
```

`crate::panama::native_access_enabled` is already `pub` (`panama.rs:224`).
Verify `RuntimeError::IllegalCallerException` is in scope in `foreign_ffm.rs`
before landing; if not, the import is `cratonvm_types::error::RuntimeError`,
which the file already uses.

**Also**: `foreign_ffm.rs`'s `reinterpret` copies `get_field(this, 0)` and
`get_field(this, 5)` straight across, which for a real heap segment copies the
**length** into the address slot. It should refuse a heap receiver by name, the
way `panama.rs` now does. Same edit, same reason.

### NOM F35-2 — `native-builtins/src/panama_libffi.rs`: `segment_address` answers 0 for a SLICED heap segment too, and 0 is right for the deref path but the accessor needs `offset - 16`

Not this lane's file. F27's arm is correct **as a deref poison** and must not
change. But `MemorySegment.address()` is a different question with a different
answer (§6.2) — measured **3** for `ofArray(new byte[32]).asSlice(3)`. This lane
fixed the accessor in `panama.rs`. Ask: add a sibling
`heap_segment_byte_start(ctx, seg) -> Option<i64>` next to
`is_real_heap_segment` so `panama.rs` and any future consumer read ONE
implementation of `offset - Unsafe.arrayBaseOffset` instead of two. This lane's
copy is `panama.rs::heap_segment_view`, and a second copy already exists in
`native-builtins/src/lang_invoke.rs::segment_raw_access` (`const ABASE: i64 = 16`),
so the rule is currently written **three** times in the workspace.

### NOM F35-3 — `native-builtins/src/lang_invoke.rs`: the VarHandle heap path assumes a `byte[]`

Not this lane's file. `segment_heap_get`/`segment_heap_set` read the backing
array one `get_array_element` per BYTE, which is right for `byte[]` and wrong
for every other backing type — an `int[]`-backed segment reads element `k`
where it should read byte `k`. Its own doc comment says so ("the common
`MemorySegment.ofArray(byte[])` case"). `panama.rs::heap_segment_read` /
`heap_segment_write` now implement the general rule with the oracle rows in
§4.3. Ask: make those two `pub(crate)` and delete `lang_invoke.rs`'s pair, or
move both into one module. Two implementations of one addressing rule, in two
files, is the drift this family keeps paying for.

### NOM F35-4 — `native-builtins/src/panama.rs`'s own four copying `ofArray` arms

This IS this lane's file and it is deliberately NOT done. `ofArray([I/[J/[F/[D)`
copy into an off-heap mirror; the oracle both refuses what that enables
(`strlen(heapSegment)` → `IllegalArgumentException: Heap segment not allowed`)
and requires the aliasing it cannot give. Converting them to the H2 alias
carrier would make all seven one family — but it would also remove the real
address Elasticsearch's bulk-vector downcalls depend on, and this lane cannot
run that workload. Whoever unifies them must decide what replaces that
capability first (`Linker.Option.critical(true)` is the JDK's answer).

### NOM F35-5 — the raw-address path has no alignment check at all

This lane's file, deliberately not done (§4.4). The oracle refuses
`arenaSeg.get(JAVA_INT, 1)` with `IllegalArgumentException` and CratonVM
performs the unaligned access. A native segment's `maxByteAlignment` is not
knowable from the carrier — the oracle answers 32 for a malloc'd one — so only
the `(address + offset) % align` half can be transplanted, and adding a
half-rule to a path that works today can only turn working calls into
exceptions. It needs the arena to record the alignment it allocated with.

### NOM F35-6 — `docs/known-issues/jdk-only/W7-89-memorysession-checkvalidstate.md`

§7.1 should NOT be marked CLOSED by F27-1 alone: §6 above shows the same repro
one `asSlice` away was still fatal afterwards. Ask: mark it **CLOSED BY F27-1 +
F35-1**, note that the `0x10` does not discriminate `length` from `offset`
(both are 16 for `new byte[16]`) and that the control flow does, and narrow
§12.4 as F27 NOM-4 asks. §7.2 is now **FIXED** (§8). §7.3 (`copyFrom` has no
registered descriptor → `AbstractMethodError`) is still open and is a good
next pick: measured, `copyFrom` and `fill` both work on heap segments on the
oracle.

### NOM F35-7 — `docs/known-issues/jdk-only/INDEX.md`

```
* `F35-1-the-segment-that-could-not-read-its-own-array-and-the-gate-that-was-inverted-20260813.md`
  — F27-1's three nominations landed, and the briefing's mode claim was half
  wrong in the half that decides whether any of it ships: `register_pe_panama`
  is synthetic-only, but FOUR of its children have a second call site in the
  always-on real-JDK registrar and one of them owns the whole `MemorySegment`
  bridge. Heap segments now read and write their backing Java array, with the
  oracle's alignment rule (both halves — `maxByteAlignment` AND the modulo),
  the oracle's exception classes, and read-only enforcement; `ofArray` covers
  all seven primitive arrays instead of four, and the three new arms ALIAS
  where the existing four COPY, because the oracle refuses the downcall the
  copy enables. F27's fix did not close W7-89 §7.1, it MOVED it: `asSlice` on
  a heap segment stamped `0 + offset` into the address slot, so
  `ofArray(new byte[16]).asSlice(3, 4).get(...)` dereferenced the literal
  address 3. And the native-access gate was INVERTED per mode — in `--jdk-only`
  the only `@Restricted` method on `MemorySegment` (`reinterpret`) ran ungated
  through `foreign_ffm.rs` while `get`/`set`, which the JDK does not restrict,
  were refused with `IllegalCallerException` (W7-89 §7.2, now fixed).
```

---

## 12. Tests

Fifteen new tests in `panama.rs`, every oracle number quoted at the site.

| test | pins | pre-fix answer |
|---|---|---|
| `a_real_heap_segment_reads_and_writes_its_backing_array` | `[4, 3, 2, 1, 0, 0, 0, 0]` after `set(JAVA_INT_UNALIGNED, 0, 0x01020304)` | array untouched (SIGSEGV, then `Null segment address`) |
| `the_array_base_offset_bias_is_removed_exactly_once` | a slice at `offset=19` writes `src[3..7]` | index 19, or refused |
| `a_non_byte_backing_array_is_addressed_by_byte_not_by_element` | `int[]`: byte 0 is element 0's low byte; byte 4 is element 1 | n/a |
| `heap_alignment_is_enforced_the_way_the_oracle_enforces_it` | all five oracle alignment rows | no check |
| `heap_refusals_use_the_oracles_exception_classes` | IOOBE / IOOBE / IOOBE / IAE, and the refused write did not happen | n/a |
| `a_zero_size_native_segment_is_also_an_index_out_of_bounds` | the raw path's class | `IllegalStateException` |
| `of_array_covers_byte_short_and_char_and_the_carrier_aliases` | byteSize, address 0, and that a write aliases — for all three | `AbstractMethodError` / real heap segment |
| `a_slice_of_a_heap_segment_is_still_a_heap_segment` | address 0, `view.start == 3`, `src[3..7]`, slice bounds | address **3**, then SIGSEGV |
| `a_native_segment_still_takes_the_raw_address_path` | **negative control** — arena round trip, and the bytes really are in the native block | same |
| `a_heap_segment_is_refused_as_an_address_value` | the named refusal, **plus a control** that a native segment still marshals | a silent `0` write |
| `the_range_test_excludes_layout_unknown` | `LAYOUT_UNKNOWN < 10` (why the old spelling was wrong), all nine value kinds, all four group kinds, and that the two sentinels differ | n/a |
| `an_unclassifiable_layout_is_refused_by_get_and_set` | both refusals name the carrier, **plus a control** that a good layout still works | `Value::Int(0)` / silent no-op |
| `slot_one_of_a_layout_is_a_long_alignment_never_an_int_size` | why the erased accessors' `Value::Int` arm could never match | n/a |
| `the_segment_accessors_are_not_gated_on_enable_native_access` | six accessors are `Ok`, **plus a control** that `downcall`'s refusal is unchanged | `IllegalCallerException` |

Four are **negative controls** rather than assertions of a fix
(`a_native_segment_still_takes_the_raw_address_path`, and the second half of
`a_heap_segment_is_refused_as_an_address_value`,
`an_unclassifiable_layout_is_refused_by_get_and_set` and
`the_segment_accessors_are_not_gated_on_enable_native_access`). Without the
first, an over-broad heap predicate would route arena memory through
`get_array_element` and every other test would still pass.

**Mutation note on the mock.** `MockNativeContext::get_field_by_name` is a
name-keyed map independent of `set_field`, so a test that only round-trips a
NAME measures the mock `[mock=slot table]`. **No assertion below is a name
lookup.** Every one is either the CONTENTS of a real `MockArray` after a write,
or an exception class. The names only supply the carrier's inputs, exactly as
the real VM's field resolver would. The strongest of them is
`the_array_base_offset_bias_is_removed_exactly_once`: dropping the `- 16` puts
the bytes at index 19, subtracting it twice makes the start negative and the
view is refused, and only the correct arithmetic puts them at 3 — the mock
cannot produce that by accident.

---

## 13. Residuals — what is left undone

1. **Nothing Rust here was built, type-checked or run.** `rustfmt` exit 0 on a
   scratch copy rules out syntax errors only. The riskiest unchecked things:
   `ctx.heap_element_type_of` and `ctx.resolve_field_index_by_class_id` (both on
   `NativeHeapAccess`, a supertrait of `NativeContext`; this file already calls
   `object_is_array`/`array_length`/`get_array_element` the same way, so the
   resolution pattern is established but these two calls are new);
   `#[derive(Clone, Copy)]` on `HeapSegmentView` (requires
   `cratonvm_types::ArrayElementType: Copy`, which it derives — checked in
   `types/src/heap_types.rs:631`); and the fifteen tests' use of
   `ctx.ensure_class_initialized(...).unwrap()` + `alloc_object`, which is the
   idiom F27's own heap test uses.
2. **Allocating an 8-field synthetic `java/lang/foreign/MemorySegment`.**
   `try_alloc_concurrent_synthetic` clamps to `num_fields.max(real)`, and the
   real class is an interface with 0 instance fields, so 8 survives. It will
   report to the layout-alias census as `Undeclared` — but so does the existing
   6-field allocation on the same class, so this adds no new species. Not
   measured.
3. **`ofBuffer(ByteBuffer.allocate(n))` is a heap segment too** (measured, §4.2)
   and is NOT registered by this file. It falls to real bytecode in `--jdk-only`
   and produces an H1 carrier, which §4 now handles — but nothing tests that
   door.
4. **`copyFrom` and `fill` on a heap segment.** Measured, both work on the
   oracle. `fill` here goes through the raw-address path and will now refuse a
   heap segment rather than crash; `copyFrom` has no registered descriptor at
   all (W7-89 §7.3, still open). Neither was extended to the heap view.
5. **`sync_heap_backed_segment` still handles only INT/LONG/FLOAT/DOUBLE.**
   Left untouched on purpose: the three new `ofArray` arms alias and have no
   mirror to sync, so nothing mints a BYTE/SHORT/CHAR-kind mirror. If NOM F35-4
   is ever landed the other way — converting the new three to copy — that
   function needs three more arms or they will silently never sync.
6. **`asSlice(long)` (the one-argument overload) is not registered by anyone.**
   In `--jdk-only` it falls to real bytecode and produces a real H1 slice, which
   §4 handles. In synthetic-JDK mode it has no implementation. Not addressed.
7. **The alignment rule is enforced only on the heap path** (§4.4, NOM F35-5).
8. **No CratonVM-side measurement of any row in this record.** Every "before"
   column is read from source; the oracle columns are transcripts
   (`scratchpad/f35/F35Probe.java`, `F35Probe2.java`).
