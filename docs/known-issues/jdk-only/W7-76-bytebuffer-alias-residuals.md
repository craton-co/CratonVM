# W7-76 — the ByteBuffer slot-alias residuals: one parity fix, two refused renumbers, and a census row that was over-reporting

Status: **source landed, NOT built and NOT run on CratonVM.** Every claim about
CratonVM is read from source and says so. Every claim about the real JDK is
`javap -p` or a transcript of a Java program on Eclipse Adoptium jdk-25.0.3.9
(`javap -version` = `25.0.3`) on this Windows host, and the probe transcript is
the only measured column in the whole record.

Branch `fix/bytebuffer-read-alias-residuals-20260812`, worktree
`C:/craton/CratonVM-bbalias-20260812`.

Inputs: W7-69-read-side-alias-instrument.md §6 items 3 and 4 (the two unrepaired
`java/nio/ByteBuffer` rows), W7-58-bytebuffer-direct-arm.md §2 and §7 (the
layout, and the registration question it surfaced and declined),
W7-68-live-under-allocations.md §3.3 (the `bigEndian` residual it routed here,
and the `HeapByteBuffer` 6-vs-11 row it reported "only so the count adds up").

---

## 1. The layout, re-derived rather than inherited

`javap -p`, transitive over the superclass chain, `static` excluded, superclass
first — the same oracle and convention as W4-4-slot-index-species-sweep.md,
W7-49-slot-index-recensus.md, W7-59-layout-detector-coverage.md and W7-69.

| slot | field | declared in | descriptor |
|---:|---|---|---|
| 0 | `mark` | `java.nio.Buffer` | `int` (private) |
| 1 | `position` | `java.nio.Buffer` | `int` (private) |
| 2 | `limit` | `java.nio.Buffer` | `int` (private) |
| 3 | `capacity` | `java.nio.Buffer` | `int` (private final) |
| 4 | `address` | `java.nio.Buffer` | `long` |
| 5 | `segment` | `java.nio.Buffer` | `MemorySegment` (final) |
| 6 | `hb` | `java.nio.ByteBuffer` | `byte[]` (final) |
| 7 | `offset` | `java.nio.ByteBuffer` | `int` (final) |
| 8 | `isReadOnly` | `java.nio.ByteBuffer` | `boolean` |
| 9 | `bigEndian` | `java.nio.ByteBuffer` | `boolean` |
| 10 | `nativeByteOrder` | `java.nio.ByteBuffer` | `boolean` |

`java.nio.HeapByteBuffer` declares **no instance fields at all** — its three
`javap` entries (`ARRAY_BASE_OFFSET`, `ARRAY_INDEX_SCALE`, `$assertionsDisabled`)
are all `static`. So `HeapByteBuffer` is 11 wide and `ByteBuffer` is 11 wide and
they are the same 11.

**No disagreement with the chain.** W7-69's `mark(0) position(1) limit(2)
capacity(3) address(4) segment(5) hb(6) offset(7) isReadOnly(8) bigEndian(9)
nativeByteOrder(10)` reproduces exactly, as does W7-58 §2's table (which reached
it through `lib/src.zip` rather than `javap`) and W7-68 §3.1's `MappedByteBuffer`
extension (`fd(11) isSync(12)`). Three routes, one answer; the third is now this
one.

The one thing worth adding to the chain, because it is what makes §4 decide the
way it does: **`bigEndian` is not merely a field, it is a field with an
INITIALISER.** `java.base/java/nio/ByteBuffer.java` declares

```java
boolean bigEndian                                   // package-private
    = true;
boolean nativeByteOrder                             // package-private
    = (ByteOrder.nativeOrder() == ByteOrder.BIG_ENDIAN);
```

so javac compiles both writes into every `ByteBuffer` constructor and nothing
else writes them. And `order()` is

```java
public final ByteOrder order() {
    return bigEndian ? ByteOrder.BIG_ENDIAN : ByteOrder.LITTLE_ENDIAN;
}
```

— `final`, reading the field directly. An object that skips the constructor
cannot have that corrected by a per-subclass override, because there is no
override point.

---

## 2. Which registrar wins, determined by reading

W7-58 §7's table is right about the tree it was written against and is **no
longer the answer for Compatible mode**, because its own §5 change moved the
gate. Re-walked here from `vm/src/vm/vm_init.rs`, following calls rather than
indentation:

| mode | the fork | what runs, in order | who owns the ByteBuffer triples |
|---|---|---|---|
| **synthetic** (`config.use_synthetic_jdk`) | `vm_init.rs:1835` `#[cfg(feature = "synthetic-jdk")]`, then `:1837` `if config.use_synthetic_jdk` | `register_builtins` (`:1839`) → `register_synthetic_overrides` → `register_s2_nio` → `register_s2_bytebuffer`; **then** `register_io_natives` (`:1840`) → `register_nio_natives` | **native-io** — it registers last, so it overwrites s2 |
| **Compatible / real-JDK**, feature-enabled build | the `else` at `:1901` | `native_methods.set_drop_real_layout_synthetic(true)` (`:1918`) → `register_essential_natives_with_shims` (`:1960`) → `register_s2_bytebuffer_essentials`; **then** `register_io_natives` (`:2157`), inside which `register_nio_natives` is **skipped** | **native-builtins (s2)** — nothing overwrites it |
| **Compatible**, default build (`#[cfg(not(feature = "synthetic-jdk"))]`, `:2472`) | no fork | same shape: `set_drop_real_layout_synthetic(true)` (`:2482`) → essentials (`:2498`) → `register_io_natives` (`:2693`); `register_nio_natives` is not even **compiled** | **native-builtins (s2)** |

The gate is `#[cfg(feature = "synthetic-jdk")] if !registry.drops_real_layout_synthetic()`
at `native-io/src/lib.rs:6798`. Both halves matter and neither is redundant:
the cfg decides what is compiled, the flag decides which class library loaded,
and `registry.rs`'s own doc for `drops_real_layout_synthetic` says so in as many
words. The flag is set **before** `register_io_natives` in both Compatible arms,
so the read at the registration site sees it.

**Therefore W7-58 §7's open question — "whether `register_nio_natives` should
still overwrite the s2 family in synthetic mode at all" — is still open, and is
now the ONLY mode where it arises.** In Compatible mode s2 already wins and
there is nothing to decide. That narrows the question from "which of two
families should own ByteBuffer" to "should synthetic mode keep preferring the
weaker of two implementations of the same thing", which is a smaller question
than the one that was recorded, and it still needs a vector that runs in
synthetic mode.

**One triple has no contest in either mode.** `java/nio/ByteBuffer.order()Ljava/nio/ByteOrder;`
is registered by s2 (`native-builtins/src/servlet.rs:5877`, inside
**`register_s2_bytebuffer`** — corrected 2026-08-12, this said
`register_s2_byteorder`, which registers only `java/nio/ByteOrder`'s statics
(`nativeOrder`, `BIG_ENDIAN`, `LITTLE_ENDIAN`) and never touches `ByteBuffer`)
and by **nobody else** — native-io registers `order`
for the six typed classes (`native-io/src/lib.rs:8715`, `:8759`, `:8813`,
`:8873`, `:8932`, `:8986`) and never for `ByteBuffer`. So `s2_bb_order` answers
`order()` in synthetic mode *and* in Compatible mode, and
`vm/src/runtime/interpreter/native_override.rs:3194` forces the native to win
over the real bytecode for that descriptor. That single fact is what makes §4
observable.

---

## 3. Per slot: reachable fallback, or dead?

The standing W4-4 remedy is name-then-slot, and every production reader in
`native-io`'s ByteBuffer family has it. So the question W7-69 §4.3 could not
answer from source — *is the numeric arm ever reached on a real receiver?* — is
answered here per slot by asking what makes the by-name arm fail. There are two
distinct ways, and conflating them is why "guarded" kept getting read as
"clean":

* the field does not resolve (a synthetic stub, no such field) — the case the
  fallback exists for; and
* the field resolves and the **value** fails the reader's own predicate — which
  a source reader who only checks "is there a by-name arm" will call guarded and
  which is in fact a live fallthrough.

| slot | site | expects | really is | reached on a REAL receiver? | why |
|---:|---|---|---|---|---|
| 0 | `bb_resolve_heap_array` | `hb` | `mark` (`int`) | **YES — the calibration case** | `hb` resolves by name on a real `DirectByteBuffer` and is **null**. That is the value-fails case, not the absent case. Slot 5 then answers null too, and slot 0 is read: `Int(-1)`, refused by the `Value::Object` match. |
| 1 | `buf_read_position` | `position` | `position` | no | declared `int`, always resolves |
| 2 | `buf_read_limit` | `limit` | `limit` | no | same |
| 3 | `buf_read_capacity` | `capacity` | `capacity` | no | same |
| 4 | `buf_read_mark` | `mark` | `address` (`long`) | **no** | `Buffer.mark` is a declared `int`; the by-name read always answers. Dead on the real layout, correct on the synthetic one. |
| 4 | `buf_set_mark` (WRITE) | `mark` | `address` | **YES, unconditionally** | it is not a fallback — the indexed write always runs. This is what the save/restore of `address` around it compensates for; see §5. |
| 4 | `bb_resolve_direct_address` | `address` | `address` | **YES, and CORRECT** | a real `HeapByteBuffer` carries `address = ARRAY_BYTE_BASE_OFFSET = 16`; `is_plausible_native_addr` refuses it, control reaches the slot read, which returns the same `Long(16)` and refuses it again. Right on both passes. |
| 5 | `bb_resolve_heap_array` | `segment` | `segment` | **YES** — and see §6.1 | reached on any receiver whose `hb` is null |
| 6 | `bb_resolve_heap_offset` | `offset` | `hb` (`byte[]`) | **no** | `offset` is a declared `int` and is `>= 0` on every real receiver, so the by-name arm's own `v >= 0` guard never rejects it |

So of the eight, three numeric arms are genuinely reachable on a real receiver
and **only one of the three is wrong** — slot 0, which W7-58 already repaired by
adding the arm below it. The other two are the deliberate controls W7-69 §3
built the instrument around, and this is the first time either has been shown
reachable rather than assumed quiet.

`buf_set_mark`'s write is the row that does not fit the "guarded is not clean"
frame at all, because it is not guarded in any sense: it is an unconditional
`Int` stamped onto a `long` pointer, and the compensation sits two lines below
it.

---

## 4. `bigEndian` — the one behaviour change, and it reaches Compatible mode

### 4.1 The RED

`ByteBuffer.allocate(8).order()` is `BIG_ENDIAN` on HotSpot, at every size and
through every factory (measured: `probes/DirectByteBufferStateProbe.java`,
`fresh.allocate8.order`, `fresh.allocate0.order`, `fresh.allocateDirect8.order`,
`fresh.wrap.order`).

`native-io/src/lib.rs`'s `alloc_byte_buffer` minted the object with a raw
`ctx.alloc_object` and ran no constructor, so `bigEndian` stayed at the Java
default `false`. `s2_bb_order` — which §2 shows owns `order()` in **both** modes
— reads it by name on any object that is not on the six-slot synthetic layout,
decodes `Int(0)` as LITTLE_ENDIAN, and answers LITTLE_ENDIAN.

**This is a Compatible-mode defect, which is why it is fixed rather than
recorded.** `register_nio_natives` is gated off the real-JDK arm, so
`ByteBuffer.allocate`/`wrap`/`slice`/`duplicate` do not reach this allocator
there — but `native-io/src/stream_decoder.rs:680` does, from the
`Channels.newReader(ReadableByteChannel, …)` branch of the StreamDecoder read
shim, and `register_stream_decoder_natives` is registered unconditionally
(`native-io/src/lib.rs:5651`, inside `register_io_natives`, which both arms
call). A Compatible-mode run therefore mints these buffers over the real
11-field `java/nio/ByteBuffer`, where `bigEndian` resolves by name and reads back
`Int(0)`.

The synthetic arm was answering BIG_ENDIAN **by accident**, and the accident is
worth naming because it is why nobody found this from a synthetic run: on a
five-slot object `s2_bb_synthetic_layout` is false (it requires exactly 6), the
class is not a `java/nio/ByteBufferAs…` view, `bigEndian` does not resolve, and
`s2_bb_order`'s last arm reads slot 0, finds the backing array `Object` there,
and falls to its `_ => 0` default, which is BIG_ENDIAN. A correct answer
produced by three failed lookups.

### 4.2 The fix

Two `set_field_by_name` writes at the end of `alloc_byte_buffer`, mirroring
`native-builtins/src/servlet.rs`'s `bb_write_hb` and
`native-io/src/direct_buffer.rs`'s `dbb_allocate_direct0`, both of which already
seed exactly this pair for exactly this reason. `nativeByteOrder` is
`bigEndian == (native order is big)`, i.e. `false` on every little-endian
target — the same value as the default, written anyway so the pair cannot drift
if a big-endian target ever appears.

| change | mode reached | justification |
|---|---|---|
| `bigEndian` / `nativeByteOrder` seed in `alloc_byte_buffer` | **Compatible AND synthetic** | genuine HotSpot-parity fix: `order()` answered LITTLE_ENDIAN where HotSpot answers BIG_ENDIAN, on a buffer real JDK bytecode receives |
| `BB_ADDRESS_SLOT` / `BB_HEAP_OFFSET_FALLBACK_SLOT` constants, `BB_SLOT_MAP` rows, comments | both, **no behaviour** | diagnostic; the two new constants are equal to the values they replace |

No `CRATONVM_*` variable was added, so the four-file flag surface
(`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
`docs/flag-tokens.md`, `docs/config/flag-inventory.md`) is untouched;
`CRATONVM_DBG_LAYOUT_ALIAS` is reused as W7-69 established.
`HEADER_SIZE` is not a factor at any site touched — every access here is by slot
index, which the object model resolves relative to the header for the caller,
and no `NativeKind` block boundary moved (nothing was registered, moved or
removed).

### 4.3 Two things the fix gets for free, and one it does not

**Free, and only because the measurement was taken first:** `slice()` and
`duplicate()` route through `alloc_byte_buffer`, and HotSpot **resets** the byte
order on both. This lane was handed "slice/duplicate/asReadOnlyBuffer must
preserve order and content" and that is half wrong: they preserve CONTENT and do
NOT preserve ORDER. Every one of them runs a `ByteBuffer` constructor, so
`bigEndian = true` runs again and the derived buffer comes back BIG_ENDIAN
however the source was set. Measured on all four derivations, on both arms —
`{direct,heap}.ord.{slice,sliceRange,duplicate,readOnly}.order` — and the
asserted expectation is BIG_ENDIAN. Asserting "preserves order" would have
pinned a behaviour HotSpot does not have, in a probe whose whole purpose is to
be the oracle.

The one exception is not an exception to the rule: `asIntBuffer()` **does** carry
the order across, because the JDK compiles one concrete view class per
endianness (`ByteBufferAsIntBufferB` / `…L`, `DirectIntBufferS`) and freezes the
order into the class at creation. `{direct,heap}.ord.asIntBuffer.fromLE` and
`.fromBE` pin both directions. `native-io`'s `tb_receiver_is_big_endian` already
decodes exactly that, from the class-name suffix, and W7-58 §4 argues why that
is decoding an authored class distinction rather than binding by name.

**Not free:** `native_bb_slice` still **copies** rather than aliases
(`native-io/src/lib.rs`, `native_bb_slice`), which W7-58 §6 named and left. The
probe's `*.slice.aliasesParent` rows are still expected red on CratonVM. Not
this lane's, and not reachable through the slot map.

---

## 5. The two mis-named slots: renumber, or name-resolution?

Both refused a renumber, for different reasons, and the reasons are on the
constants in `native-io/src/lib.rs` so the next reader does not re-derive them.

### 5.1 Slot 4 — name-resolution, and the renumber would move the disagreement

`BB_FIELD_MARK = 4` carries **one name and two beliefs**:

* `buf_read_mark` / `buf_set_mark` believe `mark`. True on CratonVM's five-slot
  synthetic layout; false on the real one, where 4 is `address`.
* `bb_resolve_direct_address` believes `address`. True on the real layout —
  *and* true on `native-builtins`' six-slot synthetic layout, where
  `servlet.rs`'s independently declared `BB_MARK = 4` carries the
  `NativeMemoryTable` pointer that `s2_bb_alloc_direct` seeds. Those buffers are
  precisely the receivers this module's direct arm was added to serve
  (W7-58 §7).

The two readers are separated **only by value type**: `Value::Int` is a mark,
`Value::Long` past `is_plausible_native_addr` is an address. That is thin, and
it is what keeps `s2_bb_set_mark` dropping the write on a direct buffer rather
than overwriting the pointer with a small integer.

Renumbering `mark` off 4 in `native-io` alone would leave `native-builtins`'
`BB_MARK = 4` where it is, on the shape both crates share. That is
W7-68 §3.2's `FileChannel` rule met in the direction where the shared index is
the **correct** one: a one-sided renumber only moves the disagreement, and it
needs both crates plus a build.

What was actually wrong is smaller and is fixed: **one index carried one name
while three call sites held two beliefs.** `BB_ADDRESS_SLOT` (`= BB_FIELD_MARK`)
now names the real meaning at the site that relies on it, and `BB_SLOT_MAP`
publishes **both** rows so the census reports the belief that is wrong and stays
clean on the one that is right.

`buf_set_mark`'s save/restore of `address` is understood, not touched, and not
deletable yet. W7-69 §6.4 proposes that "a lane repairing the slot map should
delete both together" — that is only true after the renumber, which is refused
above, so the compensation stays and its comment now says what it is
compensating for in the same words as the census row.

### 5.2 Slot 6 — neither renumbered nor deleted, and right on no layout

`bb_resolve_heap_offset`'s numeric fallback was a bare `6` expecting
`ByteBuffer.offset`. On the real layout 6 is `hb`, the backing array, and
`offset` is 7. Checked against every layout in the tree:

| layout | slot 6 holds | outcome |
|---|---|---|
| real JDK 25 | `hb`, a `byte[]` reference | refused by the `Value::Int` match — and unreachable anyway (§3) |
| CratonVM five-slot synthetic | nothing; there is no slot 6 | n/a |
| `native-builtins` six-slot synthetic | `BB_NATIVE_ID`, a `Long` alloc id (`servlet.rs:2829`) | refused by the same match |
| `native-builtins` typed views | byte start is at slot **4** as `-(start + 1)`, decoded by `s2_bb_int_byte_off` | not here |

**Not renumbered to 7**, because a second numeric guess buys nothing the by-name
arm does not already answer on every receiver that has an `offset` field at all.
**Not deleted**, because `bb_get_bulk_reads_real_heap_layout_slot_hb` in this
file's own test module drives exactly this arm — it puts the array at slot 5 and
`Int(2)` at slot 6 and asserts the window — and retiring a live test on a branch
that cannot run `cargo` is how a silent behaviour change ships. It is now
`BB_HEAP_OFFSET_FALLBACK_SLOT`, with the four-layout table above at the
declaration, and it is published in `BB_SLOT_MAP` so the sweep can finally see
it.

### 5.3 What the published map was actually getting wrong

Not "three of six slots disagree" — that part was true and is the census doing
its job. The map was wrong in **two opposite directions at once**, and both are
fixed:

1. **It was incomplete and did not say so.** The slot-6 read — W7-69 §6.3's own
   third defect — was not in the table, so `verify_declared_slot_maps` would
   have swept `java/nio/ByteBuffer` and never mentioned it. A table that looks
   total and is not reads as coverage; that is this campaign's recurring
   failure, in the shape W7-69 §5.1 caught in its own gate 6.
2. **It gave slot 4 one name where the live reader holds the other.** Publishing
   only `(4, "mark")` makes the sweep print one `wrong-field` row for slot 4,
   which reads as "the reader that reaches slot 4 is broken" — when the reader
   that reaches slot 4 on a real receiver is `bb_resolve_direct_address` and it
   is **right**. Over-reporting in the direction that costs a follow-up lane a
   look, which W7-59 §5.3 explicitly prefers to under-reporting, but not when it
   can be avoided for free.

`SlotMap::slots` is a slice, not a map, and `already_reported` dedupes on
`(class, slot, expected, site)`, so a slot may legitimately carry two rows and
they stay apart. The map now publishes eight rows and should sweep to exactly
three `wrong-field`: `0 hb→mark`, `4 mark→address`, `6 offset→hb`.

---

## 6. `java/nio/HeapByteBuffer` 6 vs 11 — repair REFUSED, and the 6 is load-bearing

W7-68 §3.3 reported this row "only so the count adds up" and routed it here.
The verdict is that it must not be changed, and the reason is not the usual
"it is clamped anyway".

**Which registrar wins.** `s2_bb_alloc` is reached from
`java/nio/ByteBuffer.allocate(I)`, registered in `register_s2_bytebuffer`. Per
§2 that registration **wins in Compatible mode** (nothing overwrites it) and
**loses in synthetic mode** to `native-io`'s `native_bb_allocate`. So the 6 is
live in exactly the mode where the class is real and 11 wide.

**Does any reader depend on 6?** Yes — and not as a width. The object is not
short: `try_alloc_concurrent_synthetic`
(`native-builtins/src/util_concurrent_ext.rs:929`) ends with
`let n = num_fields.max(real)`, so a real `HeapByteBuffer` comes back at 11,
exactly as W7-68 §1 proves for the whole `under` population. What reads the 6
is `s2_bb_synthetic_layout` (`servlet.rs:3255`):

```rust
if ctx.object_num_fields(buf) != 6 {
    return false;
}
```

— a **witness**, not a width. It is the discriminator between the six-slot
synthetic layout and everything else, and it gates the indexed fallback writes
in `bb_write_hb`, the indexed read in `s2_bb_order` and the indexed write in
`s2_bb_set_order`. Widening the request from 6 to 11 would make every
synthetic-mode ByteBuffer 11 slots wide, `!= 6` would then be true for them too,
and **every indexed fallback in the s2 ByteBuffer family would go dark at once**
— on the mode that has nothing else to fall back to.

So the row is real, permanent, and correct: a request width that is deliberately
the synthetic layout's width, on a class whose real width is larger, clamped up
by the allocator, and read back as an identity check. It is the same species as
W7-68 §3.4's `DatagramChannel` row (real and vacuous) except that this one is
real and *load-bearing*, which is a category that census has not needed until
now. A future lane that wants the row gone has to replace the witness first —
`s2_bb_synthetic_layout` asking a class-side question instead of a slot count,
the way `vm_exec.rs`'s `thread_start` was repaired (W7-69 §4.4(2), whose own
comment records that "a slot count cannot identify a layout"). That is the same
sentence, about the same kind of witness, one file over.

---

## 7. The probe

`probes/DirectByteBufferStateProbe.java`, extended rather than competed with,
and the heap control arm kept in every new section.
**156 checks before, 233 now, all green on HotSpot**, all exact values, no
range assertions and no "did-not-throw" checks. The three new sections are
appended **after** the two original arms, so the first 156 lines of
`probes/DirectByteBufferStateProbe.expected.txt` are byte-identical and the
expected file diffs purely additively — a reader can tell the new measurements
from the old at a glance.

| section | checks | what it separates |
|---|---:|---|
| `freshOrder` | 17 | the RED, stated as plainly as it can be: `allocate(8)`, `allocate(0)`, `allocateDirect(8)`, `wrap`, `wrap(a,off,len)` and its `slice()` all answer BIG_ENDIAN |
| `orderPropagation` ×2 arms | 38 | order is **reset** by `slice`/`slice(int,int)`/`duplicate`/`asReadOnlyBuffer` and **inherited** by `asIntBuffer`; content is preserved by all five |
| `arrayWindow` ×2 arms | 22 | `array()`/`arrayOffset()` on a WINDOW |

`arrayWindow` is the one that needed thought. On a fresh heap buffer
`arrayOffset()` is 0 and `array().length` is the capacity, so **a native that
answers `array()` with a fresh copy of the right size passes every check the
original arm makes**. A heap `slice()` separates them: the array is the
parent's, still 16 long, the offset is 4, and `w.array() == b.array()` is the
row that cannot be faked. The direct arm of the same battery asserts
`UnsupportedOperationException` from both accessors, and the read-only rows keep
W7-58 §6's measured asymmetry (a direct read-only view's `array()` throws
`UnsupportedOperationException`; a heap read-only view's throws
`ReadOnlyBufferException`) — now also for `arrayOffset()`, measured, same split.

Nothing in the probe asserts `remaining() >= 0` or similar; the two typed-value
rows (`getIntBE` = `462357`, `getIntLE` = `353240832`) are the same four bytes
read both ways, so a native that ignores order fails exactly one of them.

**Where to run it**: unchanged from W7-58 §7. The registrations this record is
about are `--features synthetic-jdk` + `--synthetic-jdk` for the native-io
family; under default Compatible mode the probe exercises real JDK bytecode plus
the s2 family, which is the arm §4's fix is for. Both arms are worth running and
they answer different questions.

---

## 8. New residuals found while doing this

### 8.1 `bb_resolve_heap_array` can return a `MemorySegment` as a backing array

The slot-5 probe — W7-69 §3's *deliberate non-firing control*, the one that
proves the instrument can stay quiet — is quiet for the right reason on the
receivers anyone has looked at, and is not safe in general.

Measured on HotSpot 25.0.3.9 (reflection with
`--add-opens java.base/java.nio=ALL-UNNAMED`):

| receiver | `hb` | `segment` |
|---|---|---|
| `ByteBuffer.allocate(16)` | the array | `null` |
| `ByteBuffer.allocateDirect(16)` | `null` | `null` |
| `Arena.ofConfined().allocate(16).asByteBuffer()` | `null` | `jdk.internal.foreign.NativeMemorySegmentImpl` |

On that third receiver `bb_resolve_heap_array`'s by-name `hb` read answers null,
control reaches the slot-5 probe, and `Value::Object(Some(a))` matches the
**`MemorySegment`**, which is then returned as the backing array and handed to
`ctx.heap_element_type_of` and `ctx.get_array_element`. That is a wrong-kind
object where an array is required, not a wrong slot — outside the read-side
census's vocabulary the same way an in-bounds wrong-field read is outside the
allocation census's.

**Not repaired here, and the reason is an instrument gap rather than a
judgement.** The obvious guard is to screen the slot-5 object with
`ctx.heap_kind_of(a) == ObjectKind::Array`. `native-io`'s own
`test_support::MockNativeContext::heap_kind_of` (`native-io/src/test_support.rs:673`)
returns `ObjectKind::Object` **unconditionally**, and `object_is_array` defaults
to `false` on the trait — so the screen would reject the arrays that
`bb_get_bulk_reads_real_heap_layout_slot_hb` and its siblings stash at slot 5,
and those tests would go red for a reason that has nothing to do with the guard
being wrong. Exactly the shape W7-69 §2.1 records for
`MockNativeContext::superclass_of` returning `None` unconditionally.

The honest order is: teach that mock's `heap_kind_of` to answer from its own
`HeapEntry::Array` discriminant (it already has one — `alloc_entry(HeapEntry::Array { .. })`),
**then** add the screen, in that order and preferably in that many commits.
Reachable only through `register_nio_natives`, i.e. synthetic mode.

### 8.2 A third divergent copy of the same field seed

`bigEndian`/`nativeByteOrder` are now seeded in three places that must agree and
that nothing makes agree: `native-builtins/src/servlet.rs`'s `bb_write_hb`,
`native-io/src/direct_buffer.rs`'s `dbb_allocate_direct0`, and (new)
`native-io/src/lib.rs`'s `alloc_byte_buffer`. `native-builtins/src/charset.rs:246`
and `native-builtins/src/lib.rs:5685` write `bigEndian` too. This is the
"convert the idiom, not the sites" shape at five instances, and the right fix is
one helper on `NativeContext` or in `native-api` that seeds a fabricated
`java/nio/Buffer` subclass's constructor-initialised fields. Not done here: it
crosses three crates and the point of this lane was one measurable parity fix.

### 8.3 `verify_declared_slot_maps` still has no caller

Unchanged from W7-69 §7.2, and now it matters more: the map this lane completed
is swept by a function nothing calls, so the three `wrong-field` rows §5.3
predicts are a prediction, not a transcript. The sweep needs a debug-only VM
hook after a workload, and wiring it blind is a call nobody has seen run.

**CLOSED 2026-08-12 by W7-90-slot-map-sweep-caller.md.** The trigger is the
launcher's post-`main` teardown, with the three self-terminating natives
(`System.exit`, `Runtime.exit`, `Runtime.halt`) as the second, because nothing
that exits itself reaches the first. This map's three rows are §4.1 of that
record. They are still a prediction — the sweep has a caller now, but nobody has
run it.

---

## 9. What this lane could not resolve

1. **Runtime confirmation of anything on CratonVM.** Nothing was built or run.
   The probe's oracle is measured on HotSpot; its CratonVM column has still
   never been produced — for this lane or for W7-58 or W7-68. That column is the
   single highest-value next step and it is one command in each mode.
   **Partly discharged 2026-08-12 by §10.1**: the `ord` and `win` sections are
   now asserted by a SCHEDULED fixture, so the next suite run produces that much
   of the column without anyone remembering to run a probe. §10 is what asking
   the question found.
2. **Whether the three predicted `wrong-field` rows actually print.**
   Source-level, and blocked on §8.3.
3. **Whether `register_nio_natives` should overwrite the s2 family in synthetic
   mode.** §2 narrows W7-58 §7's question to one mode and does not answer it.
   The losers are still the better code: s2 is storage-aware, its `array()`
   already throws for a direct receiver, and it seeds the order fields this lane
   had to add to the winner.
4. **`bb_resolve_heap_array`'s `MemorySegment` hazard** — §8.1, blocked on a
   mock that cannot tell an array from an instance.
5. **`buf_set_mark`'s save/restore** stays until slot 4 is renumbered in both
   crates at once, which §5.1 refuses to do one-sidedly.
6. **The five copies of the order seed** — §8.2.
7. **`native_bb_slice`'s copy-instead-of-alias** and
   `native_bb_is_read_only`'s constant `0`, both named by W7-58 §6 and still
   there. The probe's `*.slice.aliasesParent` and `*.readOnly.isReadOnly` rows
   are the discriminators and are expected red.

---

## 10. §4.3's oracle was measured; the CODE it judges was not (2026-08-12)

§4.3 is this record's best paragraph and it stops one step short. It measured
HotSpot on all four derivations, found the handed-in premise half wrong —
`slice()` / `slice(int,int)` / `duplicate()` / `asReadOnlyBuffer()` preserve
CONTENT and do **not** preserve ORDER — and wrote that down as "free, and only
because the measurement was taken first". What it never did was ask what
CratonVM's own derivations do with the order.

**They propagate it.** Four sites, all in `native-builtins/src/servlet.rs`, all
in the block whose own comment (`// slice / slice(II) / duplicate /
asReadOnlyBuffer — ALIASING views.`) describes the storage sharing and says
nothing about the order:

| registration | the line |
|---|---|
| `slice ()Ljava/nio/ByteBuffer;` | `let ord = s2_bb_order(ctx, this);`, then `ord` passed to `s2_bb_new_heap_view` / `s2_bb_new_direct_view` and `s2_bb_set_order(ctx, buf, ord)` on the synthetic arm |
| `slice (II)Ljava/nio/ByteBuffer;` | same |
| `duplicate ()Ljava/nio/ByteBuffer;` | same, plus `ctx.set_field(buf, BB_ORDER, Value::Int(ord))` on the synthetic arm |
| `asReadOnlyBuffer ()Ljava/nio/ByteBuffer;` | same |

`s2_bb_set_order`'s helper doc even names the behaviour as intended — *"Used by
the `order(ByteOrder)` native **and by slice/view creation when propagating the
source buffer's order**"* — so this is a deliberate choice made against an
un-measured belief, which is exactly the shape §4.3 congratulated itself for
avoiding one level up. `s2` is the registrar that **wins in Compatible mode**
(§2), **but only TWO of the four descriptors are on the forced-native list for
`java/nio/ByteBuffer`** — corrected 2026-08-12, the original claim said all
four. `("slice", "()Ljava/nio/ByteBuffer;")` and
`("duplicate", "()Ljava/nio/ByteBuffer;")` are present
(`vm/src/runtime/interpreter/native_override.rs:3196`, `:3197`), but
`slice (II)` and `asReadOnlyBuffer` appear **nowhere in that file** — `grep -c
asReadOnlyBuffer vm/src/runtime/interpreter/native_override.rs` is `0`. So on a
real `ByteBuffer` receiver in Compatible mode the JDK's own bytecode runs for
those two and the s2 body is never reached.

The RED is therefore live in the shipping mode for `slice()` and `duplicate()`
only. The two-line repro below is correct exactly as written — both its lines
are forced descriptors — but the `sliceRange` and `readOnly` rows of §4.3's
transcript do **not** describe a shipping-mode divergence, and §9/§11.6 should
be read with that halving in mind:

```java
ByteBuffer b = ByteBuffer.allocate(16);
b.order(ByteOrder.LITTLE_ENDIAN);
b.slice().order();      // HotSpot: BIG_ENDIAN   CratonVM: LITTLE_ENDIAN
b.duplicate().order();  // HotSpot: BIG_ENDIAN   CratonVM: LITTLE_ENDIAN
```

and every typed read through such a view is byteswapped relative to HotSpot —
the same failure mode, in the same helper family, as the Lucene
`CorruptIndexException` the comment on `s2_bb_order` records.

**Why this is a Compatible-mode carve-out and not a frozen-behaviour change.**
The value is wrong against HotSpot at every size, on both storage kinds, through
four entry points, and §4.3's own transcript is the oracle:
`{direct,heap}.ord.{slice,sliceRange,duplicate,readOnly}.order = BIG_ENDIAN`,
38 measured rows. Nothing legitimate can depend on the divergence, because real
JDK bytecode compiled against `ByteBuffer` cannot observe order propagation on a
real JVM.

**The prescription** is four lines and no new helper: replace each
`let ord = s2_bb_order(ctx, this);` with a literal `0` (this family's encoding is
`0 = BIG_ENDIAN`, `1 = LITTLE_ENDIAN` — `s2_bb_order`'s `_ => 0` default and the
`order(ByteOrder)` decode's `Some("LITTLE_ENDIAN") => 1` both pin it), and state
the reason once at the block comment. Out-of-file for the lane that found it.

`as<T>Buffer()` must **not** be changed with them: it is the one derivation that
*does* carry the order, for the reason §4.3 gives, and it reaches the order
through `s2_bb_order`'s `java/nio/ByteBufferAs…{B,L}` class-name arm rather than
through any of these four sites.

### 10.1 Where the assertions live, and what §4's own fix does NOT get

`RDirectBufferElem.derivedViewOrderIsReset()` — group 6 of
`regression-suite/src/RDirectBufferElem.java`, already in `CORE_CLASSES`, so no
`run.sh` change. 36 checks (18 per arm), every value from
`probes/DirectByteBufferStateProbe.expected.txt`: the four `.order` rows are the
RED, the `.get5` / `.getIntBE` / `.capacity` rows beside them are the guard
against a "fix" that resets the order by rebuilding the view from a fresh copy
(which would lose the aliasing §2 of W7-58 restored), and the two closing rows —
the source keeps LITTLE_ENDIAN and can be set back afterwards — are the guard
against a fix that hard-codes BIG_ENDIAN everywhere.

**§4's `bigEndian` seed is NOT covered by any scheduled fixture, and cannot be.**
Its own §4.1 says why without drawing the conclusion: the seed is in
`native-io`'s `alloc_byte_buffer`, whose only Compatible-mode entry is
`native-io/src/stream_decoder.rs`'s `Channels.newReader(ReadableByteChannel, …)`
branch. `ByteBuffer.allocate`/`wrap`/`slice`/`duplicate` do **not** reach that
allocator in Compatible mode — `register_nio_natives` is skipped there (§2) — so
`ByteBuffer.allocate(8).order()` measures `s2_bb_alloc` + `bb_write_hb`, which
has seeded the pair correctly since long before this record. The fixture's
`a fresh buffer is BIG_ENDIAN` rows are therefore a **guard**, not the
discriminator for §4; the discriminator needs a `ReadableByteChannel` and a
`Channels.newReader`, which is a different vector and a different lane.

`asIntBuffer` is also absent from the fixture, deliberately: `asLongBuffer` and
friends are not on the forced-native list, so on a real receiver those rows
measure the JDK's own bytecode rather than anything this campaign changed.

---

## 11. Triage re-read against source, 2026-08-12 (lane A24, doc-only)

This lane may not build, check, test or run anything. Everything below is read
from the working tree at `70bf05ed3`+ and says which claim it checked. It is
**verification of the record against source**, not measurement of behaviour, and
the distinction is kept in every row.

### 11.1 What is now CLOSED, and by whom

| record item | disposition | evidence read |
|---|---|---|
| §8.1 `bb_resolve_heap_array` can return a `MemorySegment` as a backing array | **CLOSED by W7-83, in the order §8.1 prescribed** | `native-io/src/lib.rs`, `bb_resolve_heap_array`: the slot-5 arm is now `if ctx.heap_kind_of(a) == ObjectKind::Array { return Some(a); }`, falling **through** rather than returning `None` — the shape §8.1 asked for. The blocker was cleared first: `native-io/src/test_support.rs:746` `MockNativeContext::heap_kind_of` now answers from its own `HeapEntry::Array`/`Object` discriminant instead of `ObjectKind::Object` unconditionally, and `heap_element_type_of`/`object_is_array` with it. §8.1 said "teach that mock's `heap_kind_of` … **then** add the screen, in that order and preferably in that many commits"; that is what happened. |
| §8.3 / §9(4) `verify_declared_slot_maps` has no caller | **CLOSED, and now gate-enforced** | one caller at `vm-cli/src/main.rs:4258`, plus a source-witness test — `native-api/tests/read_alias_coverage.rs:589` asserts the function has at least one caller outside its own module, and its message names the failure mode ("reporting zero rows means *it was never run*"). §8.3's own "CLOSED by W7-90" annotation is correct; the guard against it silently regressing is the part that was not recorded. |
| §10 the four derived views propagate the order | **LANDED for `java/nio/ByteBuffer`** | all four `s2` sites now read `let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view` — `native-builtins/src/servlet.rs:5968` (`slice()`), `:6019` (`slice(II)`), `:6069` (`duplicate()`), `:6107` (`asReadOnlyBuffer()`). The block comment at `:5919-5961` carries §10's argument, its measured oracle and its exclusion. §10 asked for "four lines and no new helper"; it is four lines and no new helper. |

### 11.2 The exclusion landed too, and it is WIDER than §10 stated

§10 closes "`as<T>Buffer()` must **not** be changed with them". True, and the
code honours it — but the sites that still propagate are not `asIntBuffer()`.
They are `slice()`, `slice(II)`, `duplicate()` and `asReadOnlyBuffer()` **on a
typed view receiver**, inside the per-width macro at
`native-builtins/src/servlet.rs:6428` / `:6465` / `:6497` / `:6523`, each still
ending `let ord = s2_bb_order(ctx, this); s2_bb_set_order(ctx, vb, ord);`
(`:6461`, `:6493`, `:6519`; `$ro` delegates to `$dup`).

Whether that is right is **not settled by §10's transcript**, which measured
`{direct,heap}.ord.{slice,sliceRange,duplicate,readOnly}` on a **`ByteBuffer`**
receiver only. `intBuf.slice()` is a different question with a different JDK
answer — the derived view is allocated as the *same* `$cls`, so on
`ByteBufferAsIntBufferL` the endianness is already frozen into the class and the
propagation is redundant rather than wrong, while on `HeapIntBuffer` `order()` is
specified as `ByteOrder.nativeOrder()` and neither the propagation nor a literal
`0` is obviously it. **No row of any probe or fixture in this tree measures
`intBuf.slice().order()`.** Recorded as an open question, not as a defect: §10's
prescription was scoped to the receiver it measured and should not be read as
having adjudicated this one.

### 11.3 `is_plausible_native_addr` — §3's row is right, and the screen has a blind spot it does not state

§3's slot-4 `bb_resolve_direct_address` row ("**YES, and CORRECT**") **verifies**.
The predicate is `v >= 0x1_0000`, declared **twice** — `native-io/src/lib.rs:8045`
and `native-builtins/src/servlet.rs:3462`, identical bodies, no shared helper —
and `native-io/src/lib.rs:24193-24194` pins both directions
(`!is_plausible_native_addr(16)`, `is_plausible_native_addr(0x1_0000)`), with a
third pin at `servlet.rs:8726` against `ARRAY_BYTE_BASE_OFFSET`. So a real
`HeapByteBuffer`'s `address = 16` is refused on both passes, exactly as §3 says.

**What the screen cannot see is a tagged arena handle, and that is the live
hazard in this family.** `Unsafe.allocateMemory` does not return a raw address:
`native-builtins/src/unsafe_natives_ext.rs:4151` declares
`pub(super) const ARENA_TAG: i64 = 1 << 62;` and `ARENA_BASE = ARENA_TAG |
0x10_0000_0000`, with the doc stating *"The tag is part of the address value
end-to-end — it is NEVER stripped"* and naming `DirectByteBuffer.address()` as
one of the paths handles flow through opaquely. Every such handle is therefore
`>= 0x4000_0010_0000_0000`, which passes `v >= 0x1_0000` **trivially**.

So `is_plausible_native_addr` separates an array-relative `Unsafe` offset from a
pointer and nothing else. It is *not* a dereferenceability test, and the value it
admits most often on a direct receiver in this VM is precisely the one value that
must not be dereferenced. The exact classifier already exists one crate over —
`unsafe_arena_addr_is_tagged(addr)` (`unsafe_natives_ext.rs:4447`, `addr &
ARENA_TAG != 0`), whose own doc says the VM's `copy_from_native_memory` bridge
classifies with it rather than with the liveness test because a *freed* handle
must not fall down the raw-pointer branch. `is_plausible_native_addr`'s two
copies consult neither.

This does **not** falsify §3 or §5.1 — both are about which slot holds what, and
both hold. It falsifies the *impression* the guard's name gives, which §5.1
leans on when it separates the two slot-4 readers "only by value type": a
`Value::Long` past this screen is not necessarily an address, it is anything at
all above 64 KiB. See NOMINATION 1.

### 11.4 Which of this record's claims are VACUOUS TESTS

Asked explicitly, because two standing arguments retire evidence in this family.

* **The `address`=16 argument.** JDK 21+ heap buffers carry `address` = 16, not
  0, so a *differential probe on the `address` field* reads green on both VMs and
  proves nothing. **No claim in W7-76 rests on such a probe** — §3's slot-4 row
  uses `address = 16` as an input to a source argument about which arm control
  reaches, not as a cross-VM comparison, and the probe rows §7 lists are
  `order()`, `array()`, `arrayOffset()` and typed values. Clean.
* **The tagged-handle argument.** Any row whose discriminator is "the address
  looked plausible" is vacuous by §11.3. **No probe row in §7 is of that shape.**
  The claim that *is* weakened is §5.1's "the two readers are separated only by
  value type", which the record already calls "thin" — it is thinner than it
  says.
* **The one genuinely vacuous row this record already flags** is §6's
  `DatagramChannel` comparison (W7-68 §3.4, "real and vacuous"), quoted rather
  than owned here.

### 11.5 Is this record's evidence SCHEDULED?

| evidence | scheduled? |
|---|---|
| `probes/DirectByteBufferStateProbe.java` — §7's 233 checks, and every CratonVM column §9(1) asks for | **NO.** The string `probes` occurs **zero** times in `regression-suite/run.sh` (checked, not assumed), at any `SUITE=` value. |
| §10.1's `RDirectBufferElem.derivedViewOrderIsReset()` | **YES.** Defined `regression-suite/src/RDirectBufferElem.java:313` and — the half that matters — **called** at `:132`, so it is not an orphan method. `RDirectBufferElem` is in `run.sh`'s `CORE_CLASSES` word list, so it runs in a plain `run.sh` and again under `CRATONVM_ARGS=--jdk-only`. |
| §5.3's three predicted `wrong-field` rows | **the sweep is now reachable** (§11.1) but nobody has run it; still a prediction. |

### 11.6 Residuals after this pass

1. **§9(1) is undischarged and is still the highest-value next step.** No
   CratonVM column has ever been produced for this probe. §10.1 discharges the
   `ord` and `win` sections into a scheduled fixture; the rest is unmeasured.
2. **§11.2's typed-view order question** — new, unmeasured, no vector.
3. **`is_plausible_native_addr`'s tagged-handle blind spot** — NOMINATION 1.
4. **§5.1's `buf_set_mark` save/restore** and **§5.2's slot-6 fallback** —
   unchanged; both refusals still stand for the reasons given.
5. **§8.2's five copies of the `bigEndian`/`nativeByteOrder` seed** — unchanged,
   still five, still nothing making them agree.
6. **§9(7)** `native_bb_slice`'s copy-instead-of-alias and
   `native_bb_is_read_only`'s constant `0` — unchanged.
7. **§9(3)**, whether `register_nio_natives` should overwrite s2 in synthetic
   mode, is **unadjudicable by any run of a shipping binary**: it lives only in
   `--features synthetic-jdk` + `--synthetic-jdk`, and a shipping binary refuses
   that mode outright. Rank it low because it cannot affect a shipped run — but
   say that, rather than recording it as merely untested.

### 11.7 NOMINATION 1 — `is_plausible_native_addr` admits a tagged arena handle

Doc-only lane; not applied. **Two files, and they must move together** — a
one-sided edit reproduces exactly the disagreement §5.1 refuses elsewhere.
Behaviour-affecting, so it wants a build and the §7 probe, not a paste.

`native-io/src/lib.rs:8045`, old:

```rust
fn is_plausible_native_addr(v: i64) -> bool {
    v >= 0x1_0000
}
```

new:

```rust
fn is_plausible_native_addr(v: i64) -> bool {
    // W7-76 §11.3. Two separate refusals, and the second is not implied by
    // the first. `v >= 0x1_0000` rejects an array-relative `Unsafe` offset
    // (`ARRAY_BYTE_BASE_OFFSET + i`), which is what a real HeapByteBuffer
    // carries in `address`. It does NOT reject an Unsafe-arena HANDLE:
    // `unsafe_natives_ext::ARENA_TAG` is bit 62, so every handle is
    // >= 0x4000_0010_0000_0000 and passes the magnitude test trivially —
    // and a handle is the ONE value in this family that must never be
    // dereferenced as a pointer (`addr=0x4000_0010_…` in a register at a
    // SIGSEGV inside a third-party `.so` IS this diagnosis). Only
    // `GetDirectBufferAddress` is audited to convert one.
    v >= 0x1_0000 && !cratonvm_native_builtins::unsafe_arena_addr_is_tagged(v)
}
```

and the identical body at `native-builtins/src/servlet.rs:3462`, where the call
is in-crate (`crate::unsafe_natives_ext::unsafe_arena_addr_is_tagged(v)`).

**Read before applying — three things this lane could not settle.**

1. **The crate dependency may not exist.** `native-io` calling into
   `native-builtins` is a direction this lane did not verify is permitted. If it
   is not, the classifier belongs on `NativeContext` or in `native-api`, which is
   the same conclusion §8.2 reaches for the order seed and is the better shape
   anyway — one predicate, two callers, no third copy.
2. **The two unit pins move.** `native-io/src/lib.rs:24193-24194` and
   `servlet.rs:8726` assert only the magnitude behaviour and stay green; a new
   pin for the tagged case is the point of the change and must be added, or the
   fix is untested by construction.
3. **Blast radius is a widening into a refusal**, in the direction the guard was
   written for: a receiver whose `address` is a live arena handle stops being
   read through `copy_from_native_memory` and degrades to this module's "no
   backing storage" error. Per the guard's own doc that is the *intended*
   degradation. But `unsafe_natives_ext.rs:1541-1551` records a case where
   clamping a live handle to 0 silently delivered zeros on every small real
   `Socket` write — so the arm that must be checked before landing is a direct
   `ByteBuffer` over an arena-backed block still reading and writing correctly,
   not just the SIGSEGV going away. §7's probe has both arms already.
