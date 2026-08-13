# ZGC reference-slot representation

**Status:** Designed, not built. Reference slots are plain pointers; nothing in
the heap stores a colored word.

This document works out what it would cost to give a load barrier a CAS-able
reference slot, and recommends an option. Nothing here is implemented. It is
the slot-shape companion to
[`zgc-jit-load-barrier.md`](zgc-jit-load-barrier.md) (which owns barrier
emission) and leans on the layout owned by
[`compact-ref-field-layout.md`](compact-ref-field-layout.md).

## 0. Verdict, up front

**The premise this analysis was commissioned under is wrong, and wrong in the
good direction.** The blocker was stated as:

> References are `Value::Object(Option<ObjectRef>)` inside a 16-byte `Value`
> cell … There is no `AtomicU64` reference slot to CAS today.

The first half is true only for one of the **four** reference-slot shapes in this
VM, and the second half is false. The measured position is:

| shape | width of the reference word | already `AtomicU64`-accessed? |
|---|---|---|
| reference **array element** | **8 bytes**, always (`REF_ELEMENT_SIZE = 8`, `types/src/heap_types.rs:176`) | no — plain `read()`/`write()` (`types/src/narrow_oop.rs:203-209`, `:216-224`) |
| **compact** reference instance field | **8 bytes** (`REF_FIELD_SIZE = 8`, `types/src/heap_types.rs:185`) | **yes** — `AtomicU64::load`/`store` (`types/src/field_layout.rs:986`, `:1047`) on the ZGC and G1 paths |
| **legacy** reference instance field | 16-byte `Value` cell, but the pointer is a **single 8-byte word at cell+8** (`types/src/heap_types.rs:230`, pinned by const-assert at `types/src/value.rs:1502-1509`) | **yes** — `read_value_atomic`/`write_value_atomic` read/write it as two `AtomicU64` words (`types/src/value.rs:1570-1573`, `:1582-1586`) |
| **static** reference field | 16-byte `Value` cell in a `StaticsBlock`, pointer at cell+8 (`vm/src/vm/realms/class_realm.rs:43-46`; JIT bakes `idx*SLOT_SIZE + FIELD_CELL_PAYLOAD64_OFFSET`, `jit/src/x64/objects.rs:361`, `:368-373`) | no |

**Every reference word in this heap is already 8 bytes wide and 8-byte
aligned.** A sibling module independently established the same thing while
sizing the remembered set: *"every reference-typed slot in the heap is 8-byte
aligned and maps to exactly one bit"* (`gc/src/zgc/remembered.rs:39-42`).

What is genuinely missing is not a representation. It is:

1. **One accessor** that hands the barrier a `&AtomicU64` for "the reference word
   of slot *i* of object *o*", covering all four shapes. `slot_as_atomic`
   (`gc/src/zgc/barrier.rs:827-831`) is ready to consume exactly that.
2. **Writer-side atomicity discipline** on that word. Mixed atomic/non-atomic
   access to the same location is a data race and UB regardless of what x86-64
   does in practice, and CratonVM violates it today in three named places (§2.3).
3. **A tag guard on the legacy arm.** A zero-filled legacy cell has discriminant
   `0` (`Value::Int`), not `4` (`Value::Object`) — object bodies are
   `write_bytes(ptr, 0, size)` (`gc/src/heap.rs:502`, `gc/src/zgc.rs:1576`) and
   nothing retypes them. Healing the payload of such a cell would produce a
   `Value::Int` holding a pointer.

**Recommendation (§5): option (e) — resolve the existing word and heal it in
place, with a per-slot fallback to non-healing.** No layout change, no new
representation, no cross-collector migration. Estimated blast radius is ~18
Rust sites in `gc/` + `types/`, not the workspace-wide rewrite the framing
feared.

---

## 1. The current representation, precisely

### 1.1 `Value` — 16 bytes, `repr(u32)`, layout pinned by const-assertion

`types/src/value.rs:65-93`:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u32)]
pub enum Value {
    Int(i32) = 0, Long(i64) = 1, Float(f32) = 2, Double(f64) = 3,
    Object(Option<ObjectRef>) = 4,
    ReturnAddress(u32) = 5, Uninitialized = 6,
}
```

`ObjectRef` wraps a `NonNull<u8>` (`types/src/value.rs:105-108`), which is
explicitly there to give `Option<ObjectRef>` the **niche optimization**: `None`
is the all-zero bit pattern, so the option is pointer-sized and tag-free
(`types/src/value.rs:100-104`).

Layout, as asserted at compile time in `types/src/value.rs:1459-1509`:

```text
 byte  0        4        8                       16
      +--------+--------+------------------------+
      |  tag   | pay32  |        pay64           |     SLOT_SIZE = 16
      | u32    | i32/f32|  i64 / f64 / *mut u8   |     heap_types.rs:171
      +--------+--------+------------------------+
        ^        ^        ^
        |        |        +-- FIELD_CELL_PAYLOAD64_OFFSET = 8   (heap_types.rs:230)
        |        +----------- FIELD_CELL_PAYLOAD32_OFFSET = 4   (heap_types.rs:226)
        +-------------------- FIELD_CELL_TAG_OFFSET      = 0    (heap_types.rs:223)

  Value::Object(Some(p))  ->  tag = 4,  pay64 = p            (value.rs:1480-1483)
  Value::Object(None)     ->  tag = 4,  pay64 = 0            (value.rs:1506-1509)
```

Three facts here are load-bearing and are **compile errors** in `cratonvm-types`
if they drift:

* the tag is a `u32` at byte 0 — `value.rs:1460-1464`;
* the `Object` discriminant is literally `4`, and the assertion message names
  `x64/objects.rs` as the consumer — `value.rs:1480-1483`;
* `Object(None)` **zeroes the payload word**, which is what lets compiled code
  null-test a reference field with a plain `cmp qword [cell+8], 0` —
  `value.rs:1506-1509`.

The last one matters more than it looks: it means the payload word of a legacy
reference cell is a *self-contained* reference representation. Null is `0`,
non-null is the bare pointer. There is no encoding to unwrap.

### 1.2 `CompactValue` — 8 bytes, NaN-boxed, and **not** a heap slot type

`types/src/compact_value.rs:229-236`:

```rust
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct CompactValue(u64);
const _: () = assert!(std::mem::size_of::<CompactValue>() == 8);
```

Encoding (`compact_value.rs:109-138`, `:186-194`):

```text
   63                    50 49  47 46                                    0
  +------------------------+------+---------------------------------------+
  |  NANBOX_BITS (1s)      | sub  |          payload (47 bits)            |
  +------------------------+------+---------------------------------------+
    NANBOX_BITS = 0xFFFC_0000_0000_0000   (:111)
    SUBTAG_SHIFT = 47, SUBTAG_MASK = 0x7  (:124, :138)
    PAYLOAD_MASK = (1<<47)-1              (:134)
    SUB_OBJECT = 2                        (:189)
```

**`CompactValue` is not the answer, and it is not even in the running.** Two
reasons, both structural:

1. **It is not a heap-slot type.** It backs the interpreter's operand stack and
   locals only;
   no heap reference field or array element is stored as a `CompactValue`
   anywhere in the tree. The heap already uses a *bare* 8-byte pointer for both
   compact fields and array elements — a strictly better representation for this
   purpose, because it needs no decode.
2. **It is structurally incompatible with a colored word.** `Z_COLORED_TAG` is
   bit 63 (`gc/src/zgc/vaddr.rs:264`), deliberately chosen so a colored word
   fails `plausible_heap_pointer`'s `<= 2^47-1` test (`types/src/value.rs:875-879`).
   But bit 63 is *also* the top bit of `NANBOX_BITS`, and a `SUB_OBJECT` payload
   is capped at 47 bits with an explicit refusal above that
   (`compact_value.rs:516-517`, `:1237`). A colored word's metadata sits at bits
   45-42 (`vaddr.rs:237-251`), inside the 47-bit payload field, so NaN-boxing a
   colored word would (a) collide the color bits with the address payload's high
   end and (b) hit the `PointerOutOfRange` refusal the moment bit 63 was set
   independently. The two encodings both claim bit 63 for different purposes.

The interaction the brief asked about therefore resolves cleanly: **there is no
interaction, because references are never NaN-boxed in the heap.** The bit-63
tag's real counterparty is `plausible_heap_pointer`, and that relationship is
exactly as `vaddr.rs:112-142` describes it.

### 1.3 A **compact** reference instance field — a bare 8-byte word

Gated by `compact_ref_fields_enabled()` (`types/src/field_layout.rs:174-185`),
a process-wide `OnceLock<bool>` that is **on by default** (`Err(_) => true`,
`:183`) and opt-out via `CRATONVM_COMPACT_REF_FIELDS=0`.

```text
object base
  |
  +-- 0 .. HEADER_SIZE(16)     ObjectHeader                 heap_types.rs:19
  |
  +-- HEADER_SIZE + layout.field_offsets[i]
      +------------------------+
      |   bare pointer (u64)   |   8 bytes, 0 = null        heap_types.rs:185
      +------------------------+
```

* Offsets come from `CompactLayout::field_offsets` (`field_layout.rs:117-134`),
  with `ref_offsets` as the GC oop-map (`:127-130`).
* The width is `FieldStorageKind::size_runtime()` = 8 for `Reference`
  (`field_layout.rs:94-99` → `narrow_oop::ref_field_size()`,
  `types/src/narrow_oop.rs:67-73`).
* **Natural alignment is guaranteed by the builder**: each field is aligned to
  `alignment_runtime()` before placement (`classloading/src/class.rs:1571-1572`,
  and the width-packing arm at `:1530`, `:1535-1536`). `HEADER_SIZE = 16` is a
  multiple of 8, so a compact reference field is unconditionally 8-byte aligned.
* The accessor is **already atomic**:

```rust
// types/src/field_layout.rs:978-993
FieldStorageKind::Reference => {
    let raw = if crate::narrow_oop::narrow_oops_enabled() {
        let n = unsafe { (&*(ptr as *const AtomicU32)).load(ordering) };
        crate::narrow_oop::decode(n)
    } else {
        unsafe { (&*(ptr as *const AtomicU64)).load(ordering) }   // <-- :986
    };
    ...
}
```

with the mirror-image `AtomicU64::store` at `field_layout.rs:1047`.

**Which objects do *not* get a compact layout** — this is the load-bearing
limitation, and it kills option (a):

| refusal | anchor |
|---|---|
| flag off | `field_layout.rs:931` |
| `class_id` slot owned by another VM's layout domain | `field_layout.rs:941-943` |
| registered layout's field count ≠ requested | `field_layout.rs:945-947` |
| no layout registered for the class at all | `field_layout.rs:945` (`with_current_class_layout` misses) |
| **the class has any padded (descriptor-less) slot** | `classloading/src/class.rs:1624-1626` |

That last one is not a corner case. Its own comment names the population:
*"the untyped `ClassId(0)`-minted synthetic containers … every HashMap/LinkedHashMap
node, view backings, …"* (`class.rs:1610-1613`). Auto-boxed primitives are also
legacy — `set_array_element` boxes into `AUTOBOX_CLASS_ID` with one field
(`gc/src/zgc.rs`), so `java.lang.Integer` is autoboxed into a legacy 16-byte
cell.

Compactness is **per object**, decided at allocation from the `GC_FLAG_COMPACT`
header bit (`field_layout.rs:1120-1122`, set at `gc/src/zgc.rs:1959-1961`), not
per class and not per process. The JIT knows this and emits a **runtime branch**
on that bit at every inline field access (`jit/src/x64/bytecode_walk.rs:4026-4032`,
`:4222-4234`; `jit/src/x64/objects.rs:203-208`).

### 1.4 A **legacy** reference instance field — 16-byte cell, 8-byte pointer word

```text
object base
  +-- 0 .. 16                    ObjectHeader
  +-- HEADER_SIZE + i*SLOT_SIZE
      +--------+--------+------------------------+
      | tag=4  | unused |     pointer (u64)      |
      +--------+--------+------------------------+
      ^                 ^
      |                 +-- HEADER_SIZE + i*16 + 8   <== the reference WORD
      +-- HEADER_SIZE + i*16                            (16-aligned base ⇒ 8-aligned)
```

`HEADER_SIZE + i*16 + 8` is 8-byte aligned for every `i` because `HEADER_SIZE = 16`
(`heap_types.rs:19`) and `SLOT_SIZE = 16` (`:171`).

Two independent confirmations that this word is already treated as an
independent atomic unit:

* `types/src/value.rs:1570-1573` / `:1582-1586` — `read_value_atomic` /
  `write_value_atomic` access the cell as **two relaxed `AtomicU64` words**.
  G1 uses them (`gc/src/g1.rs:7959`, `:8066`); Generational uses them
  (`gc/src/gen_heap.rs:15907`, and `read_value_checked_atomic` at `:15852`).
* The JIT's legacy reference `getfield` arm is a **single 8-byte `MOV` at
  `cell+8` with no tag check at all** (`jit/src/x64/bytecode_walk.rs:4076-4082`,
  and the legacy-only emitter at `:4235-4243`). The legacy reference `putfield`
  arm writes the tag and the payload as **two separate aligned 8-byte stores**
  (`:4623-4636`) — the only site in the JIT that bakes the literal `4`.

The tag word is written but never read on the load path. That is what makes
option (e) cheap: nothing on the fast path depends on the two words being
mutually consistent at any instant *except* the Rust-side `read_value_atomic`
decode, which does read both.

### 1.5 A reference **array element** — a bare 8-byte word, unconditionally

```text
array base
  +-- 0 .. ARRAY_DATA_OFFSET(16)         ObjectHeader        heap_types.rs:300
  +-- ARRAY_DATA_OFFSET + i*8
      +------------------------+
      |   bare pointer (u64)   |   REF_ELEMENT_SIZE = 8      heap_types.rs:176
      +------------------------+
```

* `element_byte_size(Reference) = narrow_oop::ref_element_size()`
  (`types/src/heap_types.rs:477`), which is `REF_FIELD_SIZE = 8` unless narrow
  oops are on (`types/src/narrow_oop.rs:77-79`, `:67-73`; default off,
  `:35`).
* Read: `gc/src/heap.rs:1687-1691` → `read_ref_slot`, which is a **plain
  non-atomic** `(*const u64).read()` (`types/src/narrow_oop.rs:203-209`).
  Write: `gc/src/heap.rs:1839-1845` → `write_ref_slot`, plain
  `(*mut u64).write()` (`narrow_oop.rs:216-224`).
* The JIT emits `MOV RAX, QWORD [RAX + RCX*8 + 16]` / the mirror store
  (`jit/src/x64/arrays.rs:71`, `:116`) — no tag, SIB scale 3, disp8 = 16.

### 1.6 A **static** reference field — a 16-byte cell outside the heap

`vm/src/vm/realms/class_realm.rs:43-46`:

```rust
pub struct StaticsBlock { ptr: *mut Value, len: usize }
```

`new(len)` is `vec![Value::Int(0); len]`, `Box::leak`ed so the base address is
stable for the VM's life (`class_realm.rs:57-67`); it is grown by allocating a
new block and leaking the old (`:86-95`). The JIT bakes
`field_index * SLOT_SIZE + FIELD_CELL_PAYLOAD64_OFFSET` for `J|D|L|[`
(`jit/src/x64/objects.rs:361`, `:368-373`).

Same shape as §1.4: 16-byte cell, 8-byte aligned pointer word at `+8`. Statics
are a **separate world** from the compact layout — the compact layout is
instance-fields-only — and any claim of the form "all reference slots are X"
requires this block to be migrated independently.

### 1.7 The one place the tag is *not* `4`

Object bodies are handed out zero-filled (`gc/src/heap.rs:502`, `:581`,
`gc/src/zgc.rs:1576`, `gc/src/tlab.rs:532`) and `alloc_object` writes only the
header (`gc/src/zgc.rs:1962-1966`). A legacy reference field that has never been
written therefore holds `tag = 0` (`Value::Int`), payload `0`. `StaticsBlock`
is likewise filled with `Value::Int(0)` (`class_realm.rs:57-67`).

**Consequence for the barrier: healing a legacy cell must be gated on
`tag == 4`.** Writing a healed pointer into the payload of a `tag = 0` cell
manufactures a `Value::Int` whose 8-byte payload is a heap address — a
type-confusion bug that `read_value_checked_atomic` (`value.rs:1654-1662`) would
happily pass, because the discriminant is in range.

---

## 2. The requirement, stated precisely

The finished barrier (`gc/src/zgc/barrier.rs`) needs exactly this, and each
clause is non-negotiable for a stated reason.

### 2.1 An 8-byte, naturally aligned location

`slot_as_atomic` (`barrier.rs:827-831`) takes a `*mut u64` and debug-asserts
`slot as usize % 8 == 0` (`:829`). Its safety comment names why: 8-byte
alignment *"on x86-64 and aarch64 is what makes the CAS single-copy-atomic in the
first place"* (`:821`).

**Non-negotiable because:** a 16-byte CAS needs `cmpxchg16b` on x86-64 and LSE
`casp` on aarch64, neither of which is baseline, and — decisively — a 16-byte
compare would include the tag word in the comparand, so a benign concurrent tag
rewrite would spuriously fail the heal. `Value` is `align_of == 8`, not 16
(`types/src/value.rs:1517-1519` pins `ObjectRef` to pointer alignment), so
`cmpxchg16b` would additionally require a layout change to `repr(align(16))`.

**Status: satisfied by all four shapes today** (§1.3-§1.6).

### 2.2 Metadata bits available in the word

The colored word is 42 bits of offset + 4 bits of color at shift 42 + bit 63
(`gc/src/zgc/vaddr.rs:26-41`, `:225-264`). That requires the slot to be able to
hold a value that is **not** a valid machine pointer.

**Non-negotiable because:** the color *is* the barrier's gate
(`barrier.rs:328-332`), and the whole self-healing scheme is "rewrite the color
in place". A slot that can only hold plausible pointers cannot hold a bad color,
and the barrier degenerates to nothing.

**Status: satisfied by the raw 8-byte word — but only if the decode paths stop
validating.** `read_prim_element`'s reference arm applies
`plausible_heap_pointer` and **degrades an implausible word to `Object(None)`**
(`gc/src/heap.rs:1692-1714`). A colored word is deliberately implausible
(`vaddr.rs:1144-1158` proves it), so under ZGC the array read path would silently
null every colored reference. This is a *feature* (`vaddr.rs:126-133` designs it
as a leak tripwire) and simultaneously the thing that makes an
un-barriered read path fail loudly rather than silently — but it means
`get_array_element` under ZGC must go through the barrier, not around it.

### 2.3 Every access to that word must be atomic

Mixed atomic / non-atomic access to one location is a data race, and a data race
is UB in Rust regardless of what the hardware does. The barrier's own safety
contract states it: *"For the duration of `'a` the pointee must be accessed
**only** through atomic operations"* (`barrier.rs:822-825`).

**Status: violated in three named places.**

| site | what it does | anchor |
|---|---|---|
| ZGC `set_field`, legacy arm | `std::ptr::write(ptr as *mut Value, value)` — a non-atomic 16-byte store that overlaps the reference word; rustc is free to lower it as one `movups` | `gc/src/zgc.rs:2074` |
| ZGC `get_field`, legacy arm | `std::ptr::read(ptr as *const Value)` — non-atomic 16-byte load | `gc/src/zgc.rs:2048` |
| reference array elements, everywhere | `read_ref_slot` / `write_ref_slot` are plain `read()`/`write()` | `types/src/narrow_oop.rs:203-209`, `:216-224` |

Generational additionally routes **compact reference fields** around the atomic
accessor: `gen_heap.rs:3518` uses `read_prim_element(base, 0, Reference)` and
`:3776` uses `write_prim_element` rather than `read_compact_field` /
`write_compact_field`, so the one shape that *is* atomic under ZGC and G1 is not
under Generational. That inconsistency is pre-existing and independent of ZGC,
but it is on the list.

The JIT is in better shape than it looks: an aligned 8-byte `MOV` is
single-copy-atomic on both supported targets, so
`jit/src/x64/bytecode_walk.rs:4034-4036` (compact ref load),
`:4076-4082` (legacy ref load), `:4536-4538` (compact ref store),
`:4623-4636` (legacy ref store, two separate aligned 8-byte stores) and
`jit/src/x64/arrays.rs:71`/`:116` all already emit the machine instruction a
relaxed atomic access would emit. **The gap on the JIT side is the barrier
sequence, not the slot width.**

### 2.4 Single-shot CAS with the exact observed word as comparand

`barrier.rs:714` uses `compare_exchange` (not `_weak`) and never retries, for
the reason spelled out at `:626-639`: a retry loop would overwrite a mutator's
store with a stale value.

**Consequence for this document:** the barrier must be handed *the same word* it
loaded, from *the same address*. Any accessor that copies the slot into a
temporary — G1's humongous path does exactly this
(`gc/src/g1.rs:7915-7944`, `humongous_copy` into a stack `[u64; 2]`) — cannot
heal, and must degrade to non-healing.

---

## 3. Options

### (a) Require the compact layout; barrier only that path

**Correctness: refused.** Compactness is a *per-object* property decided at
allocation (`field_layout.rs:1120-1122`), and a large, load-bearing population
of objects is structurally denied it: every padded/synthetic class, which the
builder's own comment enumerates as *"every HashMap/LinkedHashMap node, view
backings"* and the `ClassId(0)` synthetic containers
(`classloading/src/class.rs:1610-1626`), plus every auto-boxed primitive
(`gc/src/zgc.rs:2135-2136`). A collector that only barriers compact objects is
not a partial collector; it is a broken one, because an unbarriered load of a
relocated reference is a use-after-free.

`compact_ref_fields_enabled()` **is** a per-process runtime flag
(`field_layout.rs:174-185`, on by default), so "turn it on" is free — but
turning it on is already the default and does not make the refusals go away.
A class that is not registered simply allocates with tagged slots
(`field_layout.rs:945-947`, `gc/src/zgc.rs:1940-1944`), which is correct and
silent.

**Verdict: dead. Not a staged option, not a fallback.** Recorded here so nobody
re-proposes it.

### (b) A dedicated 8-byte reference-slot representation for all collectors

Introduce e.g. `RefSlot(AtomicU64)` and migrate every reference slot in the
workspace to it — including legacy instance fields and statics.

**Correctness:** achievable, and it is the destination the tree is already
drifting towards. `RawSlot` (`types/src/value.rs:1109-1113`) is a
`repr(transparent) u64` that already exists for the frame/operand-stack side and
whose reference encoding is *"a bare pointer; `0` is `null`"*.

**Cost:** this is the whole-workspace rewrite the framing feared, and it is
mostly *not* ZGC work:

* the legacy 16-byte cell is what makes an unregistered/padded class's mixed-type
  slots self-describing (`class.rs:1608-1623`) — removing the tag means those
  classes need a real oop-map, which is precisely what `build_compact_layout`
  refuses to fabricate because native code stores mixed types into those slots
  (`class.rs:1612-1616`);
* statics would need `StaticsBlock` to split into a value array plus a type map
  (`class_realm.rs:43-46`);
* the JIT's two-arm dispatch (`bytecode_walk.rs:4026-4032` and three siblings)
  would collapse to one arm — a real win, but only after every object is
  compact.

**Verdict: right destination, wrong first step.** It buys ZGC nothing that (e)
does not, and it blocks on a class-metadata problem (`class.rs:1608-1626`) that
has nothing to do with garbage collection.

### (c) Keep everything; make the barrier non-healing

Return the corrected address from the slow path and never CAS.

**Correctness: fine.** `load_barrier_slow` already returns `destination`
independently of whether the CAS won (`barrier.rs:714-732`), so deleting the CAS
is a two-line change that cannot introduce a correctness bug.

**Performance: quantified.** ZGC's amortization argument is that the slow path
is paid *once per slot*, not once per load (`barrier.rs:22-28`). Without healing:

* Every reference load of an unhealed slot re-enters `load_barrier_slow` —
  an `#[inline(never)]` call (`barrier.rs:643`) with, per invocation, a
  `forward()` table lookup (`barrier.rs:666`), a `mark_live()` enqueue
  (`:696`), and **four relaxed `fetch_add`s** (`:651`, `:680`, `:698`, `:716`/`:721`).
* The mark phase makes this worst-case rather than corner-case: `Remapped` is
  a **bad** color during marking (`vaddr.rs:591-597`), so *every* up-to-date
  reference in the heap trips the slow path — by design, since the barrier is
  the marking loop's work source. With healing that is one slow path per slot
  per cycle. Without it, it is one slow path **per load** for the whole mark
  phase.
* For a read-hot field in a loop the multiplier is the loop trip count. There is
  no bound on it.

**Verdict: a correct shipping intermediate, not a destination.** Its real value
is as the *per-slot fallback* inside option (e).

### (d) A side table keyed by slot address

Keep a `FxHashMap<*mut u64, u64>` (or similar) of healed values.

**Correctness: technically achievable. Operationally a known trap in this
codebase.** An address-keyed cache over heap memory needs *sweep* integration,
not a root provider — the reclaimed-and-reused address rehydrates a stale entry.
This tree has already paid for that lesson (`docs/` memory index:
"address-keyed cache needs sweep not a root provider", and the
`oscache-registry-snapshot-then-release-uaf` record).

**Performance: strictly worse than (c).** The fast path is currently a load, an
`AND` and a not-taken branch (`barrier.rs:495-503`). A side table turns it into a
hash lookup on *every reference load in the program*, which is the one thing the
module header forbids (`barrier.rs:18-21`, `:514-518`). It also reintroduces the
process-global-lock hazard `mark_live` was explicitly designed around
(`barrier.rs:320-323`).

**Verdict: dead.** It is slower than not healing at all, and it has a sweep bug
built in.

### (e) Resolve the existing reference *word* and heal it in place — **recommended**

No representation change. Add one function to the `gc` crate:

```text
ZgcRealHeap::ref_slot(&self, obj: ObjectRef, index: usize) -> Option<*mut u64>
ZgcRealHeap::ref_array_slot(&self, obj: ObjectRef, index: usize) -> Option<*mut u64>
```

resolving to, respectively:

| shape | address | condition |
|---|---|---|
| compact ref field | `obj + HEADER_SIZE + layout.field_offsets[i]` | `compact_object_field_storage(header, i)` returns `Reference` (`field_layout.rs:953-965`) |
| legacy ref field | `obj + HEADER_SIZE + i*SLOT_SIZE + 8` | tag dword at `+0` reads `4` (§1.7) |
| ref array element | `obj + ARRAY_DATA_OFFSET + i*8` | `header.element_type == Reference` |
| static ref field | `statics.base_ptr() + i*SLOT_SIZE + 8` | tag dword reads `4` |

then `slot_as_atomic` (`barrier.rs:827-831`) on the result and hand it to
`z_load` / `z_load_volatile` / `z_keep_alive` (`barrier.rs:769-794`).
`None` — a humongous G1-style fragmented slot, a legacy cell whose tag is not
`4`, an unregistered layout — degrades that *one load* to option (c): correct,
unhealed, and counted.

**Correctness:** every clause of §2 is satisfiable without moving a byte.
The alignment requirement (§2.1) is met by construction for all four shapes.
The metadata requirement (§2.2) is met because the word is raw. The atomicity
requirement (§2.3) needs the three fixes named in that section plus a rule that
ZGC's reference paths route through the atomic accessor. The comparand
requirement (§2.4) is met because the address is the real slot, not a copy.

**Interaction with compressed oops:** *verified permanently incompatible, and
already enforced.* `vm/src/vm/vm_init.rs:1391-1401` refuses to enable narrow
oops unless `gc_backend == GcBackend::Generational`, printing to stderr
otherwise; `gc/src/compressed_oops.rs` records ZGC as unmigrated. The deeper
reason `vaddr.rs:144-168` gives is correct and is the one that makes it
permanent rather than a to-do: a 32-bit slot has no room for 4 metadata bits
plus a tag, and cannot express "greater than 2^47", so the escaped-colored-word
tripwire has nothing to trip on. **Concrete follow-up:** ZGC should
`assert!(!cratonvm_types::narrow_oop::narrow_oops_enabled())` at heap
construction rather than trusting the `vm_init` check to have run — the flag is
a process-global `AtomicBool` (`narrow_oop.rs:35`) that anything can set.

**Interaction with the JIT's two lowering arms:** this is where (e) is at its
best. The JIT already branches per object on `GC_FLAG_COMPACT`
(`bytecode_walk.rs:4026-4032`, `:4222-4234`, `:4505-4511`;
`objects.rs:203-208`, `:507-512`, `:604-609`) and each arm already computes the
address of the reference word and issues a single aligned 8-byte load
(`:4034-4036` compact, `:4076-4082` legacy). **The barrier slots in after the
existing address computation, in both arms, with no change to either arm's
addressing.** That is a `test` + not-taken `jnz` + slow-path stub per arm —
exactly the shape `barrier.rs:514-518` specifies. Under option (b) the same
work would have to be done anyway, plus the arm collapse.

**Migration risk: low and bounded**, because ZGC is behind a default-off Cargo
feature (`gc/Cargo.toml`, `vm/Cargo.toml`, per
`zgc-production-implementation-plan.md` §1) and the other two collectors need no
change. The one shared change — making `read_ref_slot`/`write_ref_slot` atomic —
is a same-instruction-different-type edit on x86-64/aarch64 and is a strict
correctness improvement for Generational and G1 as well.

**The two real hazards:**

1. **The tag gate is easy to forget** (§1.7). A missing `tag == 4` check is a
   silent type-confusion, not a crash. It belongs in the `ref_slot` resolver,
   with a debug counter for refusals so "how often do we fail to heal" is
   visible rather than inferred.
2. **`read_prim_element`'s plausibility degrade** (`gc/src/heap.rs:1692-1714`)
   turns a colored array element into `Object(None)`. Under ZGC every reference
   array read must go through the barrier first; a path that reaches
   `read_prim_element` with a colored word in the slot **silently nulls a live
   reference**. That is the single most dangerous unmigrated read path, and it is
   shared with the other two collectors, so it needs a ZGC-aware branch rather
   than an edit.

### (f) Colored words only in ZGC-owned pages, plain pointers elsewhere

Not in the brief; noting it because `ZgcRealHeap` is arena-backed and
non-moving today (`zgc.rs:53-69`), so a first cut could color only objects on
`ZPage`s and leave everything else plain. **Verdict: not a separate option —
it is a *deployment* of (e)** with `ref_slot` returning `None` outside ZGC
pages, and the `None` arm is option (c). Worth naming because it is the natural
Phase-3 rollout: heal what you own, degrade elsewhere, and watch the
`heal_cas_wins + heal_cas_losses` / `slow_path_entries` ratio climb as coverage
grows.

---

## 4. Recommendation

**Take (e), with (c) as the per-slot fallback rather than as a separate phase.**

The reasoning, in order of weight:

1. **The expensive thing is already built.** Four out of four reference-slot
   shapes are 8-byte, naturally aligned words today (§0). The framing assumed a
   representation change was needed; there is nothing to change.
2. **(a) is refused by class metadata, not by GC design** (`class.rs:1624-1626`),
   so it cannot be staged into. **(d) is slower than not healing** and carries a
   sweep bug. **(b) is the right destination but blocks on an unrelated
   problem.** That leaves (e), and the elimination is not close.
3. **(c)-as-a-phase would be discarded work.** Shipping a non-healing barrier
   and *then* adding healing means writing the slot-resolution code anyway,
   later, against a barrier that has already been tuned for a slow path it will
   stop taking. Shipping (e) with a `None` arm gets the same "correct now,
   fast later" staging for the same effort, and the fallback is *per slot*
   rather than per build — so coverage grows incrementally and is measurable at
   every step.
4. **The blast radius is ~18 Rust sites** (§5), all in `gc/` and `types/`, none
   in the 6 263 `Value::Object(` sites or the 1 116 `get_field|set_field` sites
   in `vm/src` — because the `GarbageCollector` trait API is `Value`-based
   (`gc/src/collector.rs:320-329`, `:372-375`) and insulates every caller from
   the slot's shape.

### The one measurement that would confirm or kill it

**A reference-slot census on a real workload, before writing any barrier code:
what fraction of live reference slots are legacy 16-byte cells?**

How: a pair of relaxed counters in `alloc_object` on the two arms of
`gc/src/zgc.rs:1940-1944` (`compact_body.is_some()` vs `None`), weighted by the
class's reference-field count, plus an array-element counter in `alloc_array`.
Run it on `BeanRegistrationsAotContributionTests` (the workload the footprint
work already used, `value-repr-and-compressed-oops.md` §1.2) and on a
`HashMap`-heavy shape.

What it decides:

* **Legacy reference slots < ~5%** — the legacy arm of `ref_slot` can be
  deferred to a later increment, and its `None` degradation is invisible. Build
  the compact + array arms first and ship.
* **Legacy reference slots > ~25%** — the legacy arm is on the critical path
  from day one, the tag gate (§1.7) becomes a primary correctness concern rather
  than a defensive one, and the case for eventually doing (b) strengthens
  materially.

This costs about an hour, needs no barrier, and is the only number in this
document that is currently a guess. Every layout fact above is anchored; the
*population* is not.

**The second measurement, after the barrier lands**, is the one that says
whether ZGC's amortization is actually working:
`(heal_cas_wins + heal_cas_losses) / slow_path_entries` over a full mark cycle,
using the counters that already exist (`barrier.rs:152-174`). At full coverage
that ratio is 1.0. Anything materially below it is the `None` arm, and the gap
names exactly which shape is missing.

---

## 5. Blast radius

Counts from ripgrep via the Grep tool over `C:\craton\cratonvm`, 2026-08-07.
Patterns are given verbatim so the numbers are reproducible. **"Sites" means
matches, not lines.**

### 5.1 What actually has to change for option (e)

| # | Site | Anchor | Change |
|---|---|---|---|
| 1 | `read_ref_slot` | `types/src/narrow_oop.rs:203-209` | plain `read()` → `AtomicU64::load` |
| 2 | `write_ref_slot` | `types/src/narrow_oop.rs:216-224` | plain `write()` → `AtomicU64::store` |
| 3 | `read_ref_slot_unaligned` | `types/src/narrow_oop.rs:232-238` | leave (diagnostic walkers); document |
| 4 | ZGC `get_field` legacy arm | `gc/src/zgc.rs:2046-2049` | `ptr::read::<Value>` → `read_value_checked_atomic` |
| 5 | ZGC `set_field` legacy arm | `gc/src/zgc.rs:2072-2075` | `ptr::write::<Value>` → `write_value_atomic` |
| 6 | ZGC `get_field` compact arm | `gc/src/zgc.rs:2037-2044` | route reference storage through the barrier |
| 7 | ZGC `set_field` compact arm | `gc/src/zgc.rs:2057-2069` | colored store on the reference arm |
| 8-9 | ZGC `get/set_array_element` | `gc/src/zgc.rs:2099-2113`, `:2115-2145` | barrier the `Reference` element arm; must not reach `read_prim_element`'s plausibility degrade |
| 10 | ZGC `enumerate_references` compact arm | `gc/src/zgc.rs:1782-1786` | `ptr::read::<u64>` → atomic + uncolor |
| 11 | ZGC `enumerate_references` legacy arm | `gc/src/zgc.rs:1796-1801` | same |
| 12 | ZGC `enumerate_references` array arm | `gc/src/zgc.rs:1810-1817` | same |
| 13 | `read_prim_element` `Reference` arm | `gc/src/heap.rs:1687-1714` | ZGC-aware branch: a colored word must not degrade to `Object(None)` |
| 14 | `write_prim_element` `Reference` arm | `gc/src/heap.rs:1839-1846` | atomic store |
| 15 | new: `ref_slot` / `ref_array_slot` resolver | `gc/src/zgc.rs` (new) | the four-shape address resolution of §3(e) |
| 16 | new: `impl ZBarrierContext for ZgcRealHeap` | `gc/src/zgc.rs` (new) | seven methods, `barrier.rs:324-381` |
| 17 | narrow-oop refusal assert | `gc/src/zgc.rs` (new, at construction) | `!narrow_oops_enabled()` — see §3(e) |
| 18 | statics: `StaticsBlock` reference words | `vm/src/vm/realms/class_realm.rs:43-46` | atomic access to the `+8` word; **deferrable** |

**Total: ~18 sites, 15 of them in `gc/src` + `types/src`.** Two are in `vm/src`
(#18 and its JIT counterpart), zero are in `jit/src` for the *width* — the JIT's
work is barrier emission (plan doc workstream 1c), which is orthogonal to this
document.

### 5.2 Reproducible counts

| Pattern | Scope | Count | What it means here |
|---|---|---|---|
| `read_compact_field\|write_compact_field` | repo | **31** across 13 files; **~16** real call sites | the atomic 8-byte accessor pair. Call sites: `gc/src/zgc.rs:2042`,`:2062`; `gc/src/heap.rs:659`,`:692`; `gc/src/gen_heap.rs:3509`,`:3795`; `gc/src/g1.rs:7930`,`:7957`,`:8031`,`:8062`; `vm/src/jit/helpers.rs:5226`,`:5345`,`:5358`,`:5392`,`:5427`,`:5462` |
| `read_ref_slot\|write_ref_slot` | repo | **53** across 9 files | the *non*-atomic 8-byte accessor pair — the atomicity debt. `gc/src/gen_heap.rs` 16, `gc/src/gc.rs` 13, `types/src/narrow_oop.rs` 5, `vm/src/jit/helpers.rs` 6, `gc/src/heap.rs` 3, `gc/src/old_gen.rs` 3, `gc/src/concurrent_mark.rs` 3, `vm/src/memory/gc.rs` 2 |
| `as \*(const\|mut) Value` | repo | **97** across 15 files | raw 16-byte-cell accesses. `gc/src/gen_heap.rs` 18, `gc/src/gc.rs` 18, `gc/src/g1.rs` 13, `gc/src/concurrent_mark.rs` 9, `vm/src/jit/helpers.rs` 9, `gc/src/old_gen.rs` 7, `jit/src/x64/tests.rs` 7, `types/src/value.rs` 5, `gc/src/zgc.rs` **3**, `gc/src/heap.rs` 2, `vm/src/memory/gc.rs` 2, `vm/src/vm/realms/class_realm.rs` 1 |
| `FIELD_CELL_TAG_OFFSET\|FIELD_CELL_PAYLOAD32_OFFSET\|FIELD_CELL_PAYLOAD64_OFFSET` | `jit/src` | **47** across 7 files | JIT sites that know the cell layout. `bytecode_walk.rs` 18, `lib.rs` 8, `ir_lower.rs` 7, `x64/disp.rs` 7, `x64.rs` 3, `x64/objects.rs` 3, `ir.rs` 1. **None changes under (e).** |
| `REF_ELEMENT_SIZE\|ref_element_size\(\)\|REF_FIELD_SIZE\|ref_field_size\(\)` | repo | **86** across 27 files | the already-8-byte world |
| `plausible_heap_pointer` | repo | **95** across 20 files; **78** in Rust source | the tripwire bit 63 is designed to trip. `vm/src/jit/helpers.rs` 40, `types/src/value.rs` 21, `gc/src/zgc/vaddr.rs` 7, `types/src/heap_types.rs` 4 |
| `\.(get_field\|set_field\|get_field_volatile\|set_field_volatile)\(` | repo | **14 412** across 264 files | **not blast radius** — these go through the `Value`-based trait (`gc/src/collector.rs:320-329`) and are insulated |
| `\.(get_array_element\|set_array_element)\(` | repo | **3 439** across 175 files | same — insulated |
| `Value::Object\(` | `vm/src` | **6 309** across 43 files | same — insulated. Dominated by `vm/src/vm.rs` (4 918), largely test/synthetic construction |
| `GC_FLAG_COMPACT` | `jit/src` | **27** across 9 files | the per-object runtime dispatch the barrier slots into |
| `compare_exchange` | `gc/src` | mark word (`g1.rs:508`, `:560`), card table (`card_table.rs:314`,`:348`,`:694`), SATB state (`satb.rs:569`), diagnostics (`gc_quiescence.rs:290`); **zero** in `gen_heap.rs` | **no collector CASes a reference slot today** except ZGC's own new code (`zgc/barrier.rs:714`, `zgc/forwarding.rs:570`) |

### 5.3 Collector-side rewrite sites (context, not blast radius)

If ZGC ever needed the *other* collectors to understand colored words — it does
not — the cost would be: `gen_heap.rs` **6** distinct in-place reference-slot
rewrite instructions (3 in `forward_ref_slots` at `:15570`, `:15599`, `:15618`;
3 in the dirty-card fixup at `:6191`, `:6222`, `:6252`), reached from ~6 call
sites; `g1.rs` **10** distinct rewrite instructions plus 5 helper-mediated sites
(`:115`, `:118`, `:634`, `:661`, `:758`, `:786`, `:4168`, `:4177`, `:4311`,
`:8802`) across 6 functions. All are plain non-atomic stores. This is the
cross-collector cost option (b) would incur and option (e) avoids entirely.

---

## 6. Open questions

1. **What fraction of live reference slots are legacy cells?** §4's measurement.
   Nothing in the tree counts it. This is the only number in this document that
   is a guess, and it is the one that sizes the work.
2. **Do `Unsafe`-mediated reference accesses bypass the accessors?**
   `compare_and_swap_field` (`vm/src/vm/vm_exec.rs:8477`) is reported to run
   through `get_field_volatile`/`set_field_volatile` under `with_cas_lock`
   (`value-repr-and-compressed-oops.md` §1.4), which would make it barrier-safe
   by construction — but I did not read that path, and a `VarHandle` /
   `AtomicReferenceFieldUpdater` that reaches a raw slot address would be an
   unbarriered reference store.
3. **Does the striped volatile lock compose with the barrier?**
   `volatile_stripe_lock` (`gc/src/collector.rs:53-67`) exists because a 16-byte
   `Value` cell is wider than any stable Rust atomic (`:15-37`). Under (e) the
   *reference* word no longer needs it — but ZGC's `get_field_volatile` takes it
   around a call to `get_field` (`gc/src/zgc.rs:2078-2084`), so a barrier CAS
   would run inside the guard. Whether that is merely redundant or an actual lock
   inversion against `forward()` I could not determine without reading
   `zgc/forwarding.rs`, which was being written concurrently.
4. **G1's humongous field path copies the slot to a stack `[u64; 2]`**
   (`gc/src/g1.rs:7915-7944`). ZGC has no humongous path today, but if `ZPage`
   grows one, that shape cannot heal and must degrade. Unresolved because
   `zgc/page.rs` was in flight.
5. **`gc/src/zgc/remembered.rs:38` states `HEADER_SIZE = 24`.** The constant is
   **16** (`types/src/heap_types.rs:19`), and its own doc comment describes a
   "32 -> 24 shrink". Both the module comment and the constant's comment appear
   stale. The remembered-set conclusion is unaffected (16, 24 and 32 are all
   multiples of 8), but the numbers should be reconciled by whoever owns those
   files.
6. **Are there reference slots I have not enumerated?** I found four shapes.
   `RegionHeap` (`gc/src/region.rs:957`, `:993`) has its own accessors and is not
   re-exported from `gc/src/lib.rs` per
   `value-repr-and-compressed-oops.md` §1.3; JNI global/weak handle tables and
   the JIT's stack spill slots are *roots*, not heap slots, and
   `barrier.rs`'s `Root` kind (`:445-449`) already covers them as
   `&AtomicU64` — but I did not verify that every root table stores an 8-byte
   aligned word.
