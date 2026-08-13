# F37-1 — the six typed families alias, a read-only `put()` that SUCCEEDED, and the registrar that had shadowed the clearance

**2026-08-13, lane F37.** Lands F26-1's §9.1 (the six typed families still
COPIED where HotSpot aliases), §9.2 (no read-only enforcement on typed-view
writes), N1/N2 (the byte-order seed), and §9.4 items 2 and 3 (two
constants-where-the-JDK-reads-state). Plus one defect found while landing them
that is **wider and more severe than anything in the brief** (§3.1:
`ByteBuffer.allocate(8).asReadOnlyBuffer().put(0,(byte)1)` succeeded), and one
correction to F21-1 §7's registrar map (§1.2).

**Provenance: MEASURED oracle, PREDICTED VM.** Every expected value below is a
pasted transcript from `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
(Microsoft build) on this host. **This lane may not build or run CratonVM**, so
every "after" is a PREDICTION and is labelled as one. Probes:
`scratchpad/f37/F37TypedAliasProbe.java` (691 rows, all six typed families,
reflective so every family runs the same code path) and
`scratchpad/f37/FcOpen.java` (the `FileChannel.open` algebra).

What WAS executed, and it is not the VM: the five pure functions this record
adds or reuses were extracted verbatim into `scratchpad/f37/geom.rs` and run
under `rustc --test` — **21 pass, and 20 mutants, one per wrong implementation
named below, all die** (§7). All four owned files parse: `rustfmt --edition
2021 --emit stdout` on a COPY, exit 0 for each. That is a parse, not a
type-check, and neither is a run.

Files changed, all four this lane's, each path verified to exist:
`native-io/src/lib.rs`, `native-builtins/src/servlet.rs`,
`native-io/src/direct_buffer.rs`, `native-builtins/src/charset.rs`.
Everything else is a NOMINATION in §8.

---

## 1. The registrar map, RE-VERIFIED — and one correction

### 1.1 Who wins

`register()` is last-write-wins. VERIFIED by reading `vm/src/vm/vm_init.rs`:

| arm | essentials (`servlet.rs`) | `native-io` | phase 57 (`nio_file.rs`) |
|---|---|---|---|
| real-JDK A | L2055 | **L2252** | **L2339** |
| real-JDK B | L2639 | **L2834** | **L2904** |
| synthetic | L1934 (via `register_builtins`) | **L1935** | L1934 (via `register_synthetic_overrides`) |

So for the NIO buffer families **`native-io` shadows `servlet.rs` in all three
arms**, and for `java/nio/channels/FileChannel` **phase 57 shadows `native-io`
in the two real-JDK arms and `native-io` shadows phase 57 in the synthetic
one**.

### 1.2 The correction to F21-1 §7

F21-1 §7 states the synthetic-jdk arm "does **not** call
`register_essential_natives_with_shims`, so `servlet.rs`'s
`register_s2_bytebuffer_essentials` never runs". **That is false.**
`register_builtins` (`native-builtins/src/lib.rs:21601`) calls
`register_essential_natives` (L7102), which is a one-line delegation to
`register_essential_natives_with_shims(registry, ShimSelection::ALL)` (L7103),
which is the sole call site of `register_s2_bytebuffer_essentials` (L7479). So
the s2 bodies ARE registered in synthetic mode.

F21-1's **conclusion survives and is strengthened**: `register_io_natives` runs
at L1935, one line after `register_builtins`, so `native-io` wins in synthetic
mode too. The premise was wrong; the answer was right for a different reason.
This matters for §3: a lane taking F21-1 §7 at its word would conclude the
s2 bodies are live somewhere, and they are live nowhere for these descriptors.

### 1.3 Descriptor-level shadow table for the typed families

`native-io` registers, on `java/nio/{Char,Int,Long,Float,Double,Short}Buffer`
**and** `java/nio/Heap<T>Buffer`: `slice()`, `slice(II)`, `duplicate()`,
`asReadOnlyBuffer()`, `order()`, `get`×2, `put`×2, `compact`, `array`,
`hasArray`, `arrayOffset`, `allocate`, `wrap`, and the position/limit family.
`servlet.rs`'s `s2_typed_buffer_view_fns!` registers the same set **plus** the
bulk `get([XII)` / `put([XII)`.

| descriptor | live body |
|---|---|
| `slice()` / `slice(II)` / `duplicate()` / `asReadOnlyBuffer()` / `compact()` / `put(X)` / `put(IX)` | **`native-io`** |
| bulk `put([XII)` / `get([XII)` | **`servlet.rs`** (native-io registers no bulk typed accessor) |
| `ByteBuffer.as<T>Buffer()` | **`servlet.rs`** (native-io registers no `as<T>Buffer`) |
| `ByteBuffer.put([B)` | **`servlet.rs`** (native-io registers `put([BII)` but not `put([B)`) |
| everything else on `ByteBuffer` | **`native-io`** |

Both crates are edited accordingly, and each edit says which side of that line
it is on.

---

## 2. Task 1 — the six typed families now ALIAS

### 2.1 The contract, MEASURED

`<fam>Buffer.allocate(8)` filled `10..17`. `array` is compared by **identity**
(`==` against the source array), never by equality. All six families
(`Int`/`Long`/`Short`/`Char`/`Float`/`Double`) were run through the SAME
reflective code path and the transcripts **mechanically diffed after
normalising the family name**: the only differences are element-width
artefacts of the probe (a 64-byte `ByteBuffer` is 16 ints / 8 longs / 32
shorts) and `0` vs `0.0` float formatting. **Structurally identical — every
class name, every flag, every exception class, every offset.**

#### Heap source, writable

| derived from `<fam>.w` | class | `isReadOnly` | `isDirect` | `hasArray` | pos/lim/cap | `array()` | `arrayOffset()` | shared |
|---|---|---|---|---|---|---|---|---|
| `.position(2).slice()` | `Heap<fam>Buffer` | false | false | true | 0/6/6 | **SAME** | **2** | **BOTH ways** |
| `.slice(3,4)` | `Heap<fam>Buffer` | false | false | true | 0/4/4 | **SAME** | **3** | — |
| `.duplicate()` | `Heap<fam>Buffer` | false | false | true | 2/8/8 | **SAME** | 0 | — |
| `.slice().slice(1,2)` | `Heap<fam>Buffer` | false | false | true | 0/2/2 | **SAME** | **3** | — |
| `.slice().duplicate()` | `Heap<fam>Buffer` | false | false | true | — | — | **2** | — |
| `.asReadOnlyBuffer()` | `Heap<fam>BufferR` | **true** | false | false | 2/8/8 | ROBE | ROBE | **read-through** |

```text
<fam>.w.pos(2).slice write seen by src.get(2)   99
<fam>.src write seen by slice.get(1)            77
<fam>.w write seen by aro.get(5)                55
<fam>.allocate(4).pos(4).slice().arrayOffset    4      <- EMPTY slice, offset still moves
<fam>.allocate(4).pos(4).slice() geom           0/0/0
```

`ROBE` = `java.nio.ReadOnlyBufferException`, `getMessage()` null on every row.

#### Heap source, read-only

`<fam>.ro.slice` / `.duplicate` / `.asReadOnlyBuffer` / `.slice(1,2)` are all
`Heap<fam>BufferR`, `isReadOnly == true`, `hasArray == false`, and both
`array()` and `arrayOffset()` raise **ROBE**.

#### `ByteBufferAs<T>Buffer` view source

| derived from `bb.as<T>Buffer()` | class | `isReadOnly` | `isDirect` | `hasArray` | `array()`/`arrayOffset()` |
|---|---|---|---|---|---|
| the view itself | `ByteBufferAsIntBufferB` | false | false | false | **UOE**, msg null |
| `.position(1).slice()` | `ByteBufferAsIntBufferB` | false | false | false | UOE |
| `.duplicate()` | `ByteBufferAsIntBufferB` | false | false | false | UOE |
| `.asReadOnlyBuffer()` | `ByteBufferAsIntBufferRB` | **true** | false | false | **UOE** |
| `roBB.as<T>Buffer()` | `ByteBufferAsIntBufferRB` | **true** | false | false | **UOE** |

`UOE` = `java.lang.UnsupportedOperationException` — the array-less arm is asked
FIRST, so a read-only ARRAY-LESS view answers UOE and **not** ROBE. `order()`
on the view is `BIG_ENDIAN`. Writes through a slice of the view reach the
backing `ByteBuffer` (`view slice write seen by bb: yes@7`).

#### Direct source

| derived from `allocateDirect(64).as<T>Buffer()` | class | `isDirect` | `isReadOnly` |
|---|---|---|---|
| the view | `DirectIntBufferS` | **true** | false |
| `.slice()` / `.duplicate()` | `DirectIntBufferS` | **true** | false |
| `.asReadOnlyBuffer()` | `DirectIntBufferRS` | **true** | **true** |

and `direct.view.slice.get(0)` reads back the value written through the source
view — **direct derivations stay SHARED**.

#### `wrap`

```text
<fam>.wrap(a).array()==a         true      <- IDENTITY
<fam>.wrap(a) geom               0/8/8
<fam>.wrap(a).arrayOffset        0
<fam>.wrap(a).put(0,88) -> a[0]  88        <- reaches the CALLER's array
<fam>.wrap(a,2,3) geom           2/5/8     <- off/len move POSITION and LIMIT
<fam>.wrap(a,2,3).arrayOffset    0         <- and NOT the array base
<fam>.wrap(a,2,3).slice().arrayOffset  2
```

### 2.2 What the VM did

`tb_abstract_view_fns!`'s `$slice_fn`/`$slice2_fn`/`$dup_fn` allocated a fresh
typed buffer and copied elements. The same three properties a copy cannot
express that F26-1 §2 records for `ByteBuffer`, one family per element type:
`array()` is a private array of the wrong length, `arrayOffset()` can only be
`0` *unreachably*, writes through the derived buffer are LOST, and a DIRECT
source came back as a HEAP copy so `isDirect()` flipped `true` → `false`.

### 2.3 The two mechanisms, and the third that must REFUSE

F26-1 §9.1 named this as the reason it stopped, and the shape it named is the
shape that is here. Three storage encodings live behind one
`java/nio/IntBuffer` stamp:

| receiver | storage encoding | this lane |
|---|---|---|
| `alloc_typed_buffer`-minted (`IntBuffer.allocate`/`wrap`) | `hb` + `offset` in **ELEMENTS** | **aliases** |
| a DIRECT view (`allocateDirect(n).asIntBuffer()`) | `address`, bytes | **aliases** |
| an s2 heap view (`allocate(n).asIntBuffer()`) | `servlet.rs`'s `BB_SEGMENT_SLOT` array + a byte start encoded as `-(bs+1)` in the MARK slot | **REFUSES → copy** |

`tb_alias_source` is the discriminator, and it applies three screens:

1. **`hb` BY NAME**, not `bb_resolve_heap_array` — whose second arm reads
   `BB_SEGMENT_SLOT`, i.e. exactly the encoding that must be refused.
2. **the array's element type equals the family's** — an s2 view's array is the
   source `ByteBuffer`'s `byte[]`, so a `java/nio/IntBuffer` stamp over a
   `byte[]` is the third encoding arriving by another door.
3. **the resolved storage agrees with the arm** — the window must not be
   computed against one storage and installed over another.

**Refusing rather than guessing is the design**, per F26-1's own wording: a
wrong guess here reads the WRONG ELEMENTS silently, where a copy merely loses
aliasing — which is the defect that already existed. In synthetic-JDK mode
`hb` does not resolve and `offset` is not representable, so every receiver
refuses and the copy path is unchanged there.

### 2.4 The one thing that needed the element width

**F26's `slice_window` / `slice_range_window` / `duplicate_window` are reused
UNCHANGED.** Real `HeapIntBuffer.slice()` is
`new HeapIntBuffer(hb, -1, 0, rem, rem, pos + offset, segment)` — the same five
arguments as `HeapByteBuffer.slice()`, with `offset` counted in elements. The
arithmetic is unit-agnostic; §7's fixtures are what justify the reuse rather
than assuming it.

The width enters in exactly one place, `typed_buffer_address`:

```java
// jdk25src/java.base/java/nio/HeapIntBuffer.java:111
this.address = ARRAY_BASE_OFFSET + off * ARRAY_INDEX_SCALE;
```

`ByteBuffer`'s scale is 1, so `heap_buffer_address` can add `offset` directly;
a `LongBuffer` view at element offset 3 has `address == 16 + 24`. Reusing the
byte function here would not throw — it hands `ScopedMemoryAccess` an
array-relative offset short by a factor of the element width, and every bulk
transfer through the view reads the wrong elements silently. This VM publishes
16 as the base offset of every primitive array, so only the scale differs.

### 2.5 Landed vs. specified-but-unlanded, per member

**Landed for all six families** (`native_cb_*`, `native_ib_*`, `native_lb_*`,
`native_fb_*`, `native_db_*`, `native_sb_*` — one macro, six expansions, no
member special-cased):

* `slice()` — aliases heap and direct, offsets compose, empty slice still moves
  the offset;
* `slice(int,int)` — aliases, absolute-indexed (`position` ignored), bounds
  check still runs before anything allocates;
* `duplicate()` — aliases, **carries the source's `offset`** (the typed twin of
  the defect F26-1 §3 found live on dev) and keeps the mark;
* `asReadOnlyBuffer()` — unchanged in shape; it is `$dup_fn` + `ForceReadOnly`
  and therefore inherits the aliasing automatically.

**Not landed, and named:** the s2-heap-view encoding (row 3 of §2.3) still
copies. It is a REFUSAL by construction, not an omission, and the refusal is
what F26-1 asked for. Making it alias needs `bb_state` to understand the
`-(bs+1)` marker and the element-kind mismatch it implies, which is a change to
how the two crates agree about one object's layout — a different job, and one
that cannot be validated without a build. §8 N4.

**A pre-existing defect this refusal does NOT fix, disclosed:** for an s2 heap
view receiver, `bb_state` already reports `elem == Byte` (from the `byte[]`) for
an object stamped `java/nio/IntBuffer`, so the COPY path reads bytes where it
should read ints. That is wrong today, was wrong before this change, and is
untouched by it. It is the same encoding-mismatch this lane refuses to alias
over, and it is why refusing is right: aliasing would have converted a
copy-shaped wrong answer into an alias-shaped one.

### 2.6 Two GC hazards found while landing

Both in `tb_abstract_view_fns!`, both pre-existing:

1. **`buf_stamp_read_only(ctx, this, new_buf, …)` ran AFTER
   `alloc_typed_buffer`**, i.e. it read a field off `this` across two
   allocations that can relocate it. Replaced by reading the flag into a `bool`
   BEFORE deriving and calling `buf_write_read_only` — the same fix, for the
   same reason, that `native_bb_slice` already had.
2. **The copy loop read through a `TbView` captured before the allocation.**
   `alloc_typed_buffer` allocates twice; a collection in between left
   `BbStorage::Heap { arr }` pointing at a relocated array. `tb_alloc_copy_target`
   pins it and rebuilds the view, which is what `bb_derive_view`'s fallback
   already did for the byte half.

---

## 3. Task 2 — a read-only `put()` that SUCCEEDED

### 3.1 The finding that is wider than the brief

The brief describes this as "a new throw on four paths across five classes" in
`s2_typed_buffer_view_fns!`. **Three of those four paths are shadowed** (§1.3),
and the live bodies had a hole the brief does not mention:

**`native-io` had ZERO read-only checks on ANY write path — including
`ByteBuffer`'s.** `native_bb_put`, `native_bb_put_abs`, `native_bb_put_bulk`,
`native_bb_put_bb`, `native_bb_put_int`/`_long`/`_short`/`_float`/`_double`/
`_char` (+ the `_abs` variants) and `native_bb_compact` all wrote straight
through. `("java/nio/ByteBuffer", "put", …)` is in
`force_native_over_real_jdk_bytecode`'s list AND registered last by
`register_io_natives`, so on a genuine real-JDK `HeapByteBufferR` receiver
reached through a `ByteBuffer`-typed call site:

```java
ByteBuffer.allocate(8).asReadOnlyBuffer().put(0, (byte) 1)   // SUCCEEDED
```

The byte landed in the source's backing array, with no exception at any point,
by following the documented protocol on a receiver whose entire contract is
that it cannot be written. That is a **wrong CAPABILITY, silent** — this
codebase's most severe triage class.

`servlet.rs`'s `ByteBuffer` write natives have carried this guard since F5 (16
sites, all `ReadOnlyBufferException`), and **F21-1 §8 CLEARED the cell
`<fam>.ro.compact()` on the strength of them.** That clearance was measured
against a shadowed body. Third instance of F21-1's own registrar finding, and
the first where it hid a live defect rather than a dead fix.

### 3.2 The exception class, MEASURED — and a correction to the brief

The brief says: *"For the typed views the array-less check runs first, so the
answer is `UnsupportedOperationException`, not `ReadOnlyBufferException` —
verify which applies where."* **Verified, and it does not apply to writes.**

The array-less-first ordering is specific to `array()` / `arrayOffset()`
(`buffer_array_access`'s contract, F14-1's table). JDK 25's
`ByteBufferAsIntBufferRB.put(int)` is a bare `throw new
ReadOnlyBufferException();` with no test above it
(`jdk25src/java.base/java/nio/ByteBufferAsIntBufferRB.java:166`), and so is
`HeapIntBufferR`'s. MEASURED — **`ReadOnlyBufferException`, `getMessage()`
null, on every write path of every family and every read-only receiver shape,
including the array-LESS ones**:

```text
bb.ro.put(b) / put(i,b) / put(byte[]) / put(byte[],0,2) / put(ByteBuffer)   ROBE msg=null
bb.ro.putInt / putInt(0,i) / putShort / putChar / putLong / putFloat / putDouble  ROBE
bb.ro.compact()                                                            ROBE
bb.ro.duplicate().put(b) / bb.ro.slice().put(b)                            ROBE
bb.roDirect (java.nio.DirectByteBufferR) .put(b) / .compact()              ROBE
<fam>.ro.put(x)/put(i,x)/put(arr)/put(arr,0,2)/put(buf)/compact()          ROBE
<fam>.roView (ByteBufferAs<T>BufferRB) .put(x)/put(i,x)/put(arr,0,2)/compact()  ROBE
<fam>.roDirectView (Direct<T>BufferRS) .put(x)                             ROBE
<fam>.ro.duplicate().put(x) / <fam>.ro.slice().put(x)                      ROBE
Char.ro.put(String) / Char.ro.append(CharSequence)                         ROBE
CharBuffer.wrap("hello")  ->  java.nio.StringCharBuffer, isReadOnly true, put(c) ROBE
```

**The negative controls**, which is what makes the guard falsifiable rather
than a blanket refusal:

```text
bb.w.put(b)      java.nio.HeapByteBuffer[pos=1 lim=8 cap=8]     <- still writes
bb.w.compact()   java.nio.HeapByteBuffer[pos=8 lim=8 cap=8]
<fam>.ro.get()   still works                                     <- reads unaffected
```

`ReadOnlyBufferException` **extends** `UnsupportedOperationException`
(`jdk25src/.../ReadOnlyBufferException.java:40`; MEASURED
`ROBE.getSuperclass() == java.lang.UnsupportedOperationException`,
`ROBE instanceof UOE` true, `UOE instanceof ROBE` false), so an
`instanceof`-shaped or `matches!`-shaped assertion on this pair discriminates
in ONE direction only. Every assertion this lane writes compares an exact
class name or an exact string.

### 3.3 What landed

**`native-io/src/lib.rs`** — one shared `pub fn buffer_check_writable(ctx, buf)`
next to `buffer_no_backing_array`, called as the FIRST statement of **26 write
natives**: `native_bb_compact`, `native_bb_put`, `_put_abs`, `_put_bulk`,
`_put_bb`, `_put_int`, `_put_int_abs`, `_put_long`, `_put_short`, `_put_float`,
`_put_double`; `native_cb_put`, `_put_abs`, `_put_string`, `native_cb_compact`;
`native_tb_compact`, `native_tb_put_{int,long,float,double,short}` and their
five `_abs` twins. (`native_bb_put_char` delegates to `native_bb_put_short` and
is covered through it; `native_bb_put_float`/`_double` also delegate to
`_int`/`_long` and are guarded at both ends, which is idempotent.)

**`native-builtins/src/servlet.rs`** — `s2_typed_buffer_view_fns!`'s `$put`,
`$put_abs`, `$put_bulk` and `$compact`, i.e. F26-1 §9.2 exactly. `$put_bulk`
(`put([XII)`) is the one of the four that is **NOT shadowed** and therefore the
one whose repair is reachable today; the other three are landed anyway,
because a family half-fixed is the inconsistency.

### 3.4 …and the three things that would have walked around it

A guard on the receiver is not a guard if one more call hands back a writable
alias. All three of these are the "capability re-opens one call later" shape
F21-1 closed for `ByteBuffer`, and all three would have been re-opened HERE by
the very change that closed the direct route:

1. **`s2_typed_buffer_view_fns!`'s `$ro` was a plain `$dup(ctx, args)`** —
   `asReadOnlyBuffer()` returned a WRITABLE alias (F26-1 §9.4 item 1). Now
   `$dup` + an unconditional `s2_bb_set_read_only(…, true)`.
2. **`$slice`/`$slice2`/`$dup` did not propagate the flag.** Now they read the
   source's flag and stamp it (`Inherit`), per F21-1 §1.1's contagion table.
3. **`s2_view_buf_fn!` (`ByteBuffer.as<T>Buffer()`) did not propagate either** —
   F21-1 N3's other half, and **this one is LIVE in every mode** (`native-io`
   registers no `as<T>Buffer` descriptor). Without it: take a read-only
   `ByteBuffer`, ask it for an `asIntBuffer()` view, write through the view
   into the read-only buffer's own backing array. MEASURED:
   `allocate(64).asReadOnlyBuffer().asIntBuffer()` is a
   `java.nio.ByteBufferAsIntBufferRB` with `isReadOnly() == true`.

Plus, in `s2_bb_as_char_buffer` (a hand-written sibling of that macro, so it
was not covered by any of the above):

4. its real-JDK arm read the flag as an open-coded
   `matches!(…, Value::Int(1))` — the **fifth** transcription of the
   `isReadOnly` field read and the only one testing `== 1` rather than `!= 0`;
   routed through the converged `s2_bb_is_read_only`. Latent divergence closed,
   not a flip.
5. its transcoding FALLBACK arm wrote a flat `isReadOnly = 0`, i.e. it
   **actively cleared** the flag — the same shape F21-1 §6.2 found in
   `slice`/`slice(II)`/`duplicate`'s copying arms, in the one family member
   nobody had re-read. Now propagates, and the source's flag is read into a
   `bool` BEFORE the allocation so no `ObjectRef` crosses it.

---

## 4. Task 3 — W7-76 §8.2's seed, two more sites converged

| site | before | after |
|---|---|---|
| `native-io/src/direct_buffer.rs:743` (`dbb_allocate_direct0`) | both writes, open-coded | `crate::seed_buffer_byte_order(ctx, buf)` |
| `native-builtins/src/charset.rs:259` (`alloc_char_buffer`) | **`bigEndian` only — already DRIFTED** | `cratonvm_native_io::seed_buffer_byte_order(ctx, obj)` |

The first is byte-for-byte identical to what it replaces (same two names, same
two values, same `cfg!(target_endian)` test) and is therefore a **no-op today
by construction** — which is the point: the copy it removes is one that could
drift, and its sibling already HAD.

The second is the drift §8.2 predicted, sitting there before anyone looked:
`java.nio.CharBuffer`'s field initialisers are `bigEndian = true` and
`nativeByteOrder = (ByteOrder.nativeOrder() == BIG_ENDIAN)`, javac compiles
BOTH into every constructor, and this allocator runs no constructor — so
`nativeByteOrder` stayed at the Java default `false`. On a little-endian host
that is the value it wants, so the omission was **inert here** and stops being
inert the moment a reader consults `nativeByteOrder` or a big-endian target
appears. Closed by construction rather than by a fourth literal.

The third site is `native-builtins/src/lib.rs:5690`, which this lane does not
own: **§8 N1**.

---

## 5. Task 4 — the constant-where-state sweep, both taken

### 5.1 `FileChannel.size()` — and the reachability the brief did not have

`native_fc_size` resolved the fd, discarded it (`let _ = fd_id;`) and returned
`Ok(Some(Value::Long(0)))`. Real `FileChannelImpl.size()` fstats; callers size
their reads and their `map()` regions from the answer, so every one of them saw
an EMPTY file.

**But the flat `0` was DEAD in the shipping modes.** `p57_fc_size`
(`native-builtins/src/phases_late/nio_file.rs:15112`) registers the same
descriptor and already fstats, and phase 57 runs AFTER `register_io_natives` in
both real-JDK arms (§1.1). It is live in **synthetic-jdk mode only**, where the
order reverses. Fixed anyway — one live mode is still a mode, and both bodies
now answer from the same `FileDescriptorTable::file_size`, whose own doc
comment names `FileChannel.size()` as its caller and which handles all three
file entry kinds (flushing a `FileWrite` first so buffered bytes count) with
cursor save/restore. The outcome no longer depends on which registrar ran last.

F26-1 §9.4 item 2 said "which wins is a registrar-order question of exactly the
§2 species and should be settled by reading `vm_init.rs`, not assumed". It was;
the answer is the opposite of what the item's severity implied, and the item is
right that it had to be read.

### 5.2 `FileChannel.open(Path, OpenOption[])` — the options were discarded

The parameter was documented `(ignored)` and the body carried
`// Simplified: open for read`. `FileChannel.open(p, WRITE, CREATE)` handed
back a READ-ONLY fd and the first `write()` failed at the fd layer with a "bad
fd"-shaped `IOException` naming nothing the caller had done wrong.

**This native is NOT shadowed** — VERIFIED by enumerating every
`register(fc_cls, …)` in `nio_file.rs` (`size`, `position` ×2, `close`,
`isOpen`, `write`, `read` — no `open`). It is the live body in every mode.

MEASURED (`scratchpad/f37/FcOpen.java`):

```text
open(p)                             READ; size()==5; write() -> NonWritableChannelException
open(p, READ, APPEND)               java.lang.IllegalArgumentException: READ + APPEND not allowed
open(p, APPEND, TRUNCATE_EXISTING)  java.lang.IllegalArgumentException: APPEND + TRUNCATE_EXISTING not allowed
open(missing, WRITE)                java.nio.file.NoSuchFileException
open(missing, WRITE, CREATE)        ok, size()==0
open(p, WRITE) then write(1 byte)   file length still 5
```

**That last row is the one a plausible implementation gets wrong.** WRITE alone
neither truncates nor creates — and `FileDescriptorTable::open_write` is
`.create(true).truncate(!append)`, i.e. it does BOTH unconditionally. The naive
"writable ⇒ `open_write`" mapping would silently destroy the contents of a file
opened `WRITE` with no other option.

Landed: a pure `file_channel_open_mode(&[&str]) -> Result<FileChannelOpenMode,
&'static str>` carrying the algebra and the two refusals with the JDK's exact
detail messages; option names decoded through the same
`read_afc_open_option_name` / `normalize_afc_open_option_name` pair
`AsynchronousFileChannel.open` uses, so an enum constant resolves identically
for both entry points. Mapping onto `fd_table`:

| mode | call | exact? |
|---|---|---|
| read-only | `open_read` | yes |
| writable, no APPEND/TRUNCATE | `open_read_write(path, create)` | yes — `create` exactly as asked, no truncate |
| WRITE+TRUNCATE_EXISTING | `open_write(path, false)` | exact for the `+CREATE` set; over-eager create without it |
| APPEND | `open_write(path, true)` | exact for the `+CREATE` set; over-eager create without it |

Both residuals are one thing: `fd_table` has no "open for write without
creating" entry point, and `native-api/src/fd_table.rs` is not this lane's
file. **§8 N2.** Also disclosed and NOT fixed: a writable channel is opened
`.read(true)` too, so `NonWritableChannelException` /
`NonReadableChannelException` are still unenforced — a missing capability this
change neither adds nor worsens, since before it every channel was read-only.

Unrecognised options are **ignored, not refused** — deliberately different from
`parse_afc_open_options` next door, which really does reject what it cannot
honour. `FileChannel.open` accepts `SPARSE`/`SYNC`/`DSYNC`/`DELETE_ON_CLOSE`
and provider-specific `ExtendedOpenOption`s and this VM honours none of them;
refusing would turn a working open into a failure, where ignoring loses only a
durability hint.

### 5.3 `ServiceLoader.toString()` — NOT taken, and why

F26-1 §9.4 item 5 recorded "verify the slot first — a plausible-looking wrong
slot here is the `_ =>`-default shape". **Respected: not touched.** Verifying
which slot holds the service mirror needs either a run or a reading of every
writer of `java/util/ServiceLoader`'s two synthetic fields, and `reload()`
clearing slot 1 is a hint rather than a verification. Left for a lane that can
build. Note in passing that `D1-R11-SERVICELOADER-DOUBLE-SOURCE-20260813.md`
exists and may already have the answer; it was not read by this lane and is not
being cited as if it were.

---

## 6. Swept and CLEARED — checked, not changed

| cell | verdict |
|---|---|
| `servlet.rs`'s 16 `ByteBuffer` write natives | all already guard on `s2_bb_is_read_only`; correct, and all but `put([B)` shadowed |
| `native_tb_array` / `native_bb_array` / `arrayOffset` / `hasArray` | already route through `buffer_array_access`; the UOE-before-ROBE ordering matches §2.1's view rows exactly |
| ROBE detail message | `RuntimeError::ReadOnlyBufferException` → no message; MEASURED null on every one of ~60 ROBE rows |
| `order()` on derived typed buffers | untouched; the `<fam>` heap families' own order is `nativeOrder()` and `asXBuffer()` carries the source's — both F21-1's, reproduced only as controls |
| `$ro_fn` in `tb_abstract_view_fns!` | `$dup_fn` + `ForceReadOnly`; unchanged, and it inherits the new aliasing for free |
| `s2_bb_as_char_buffer`'s real-JDK arm | already propagated read-only; only its fallback did not (§3.4) |

---

## 7. What was executed, and the mutants

The five pure functions (`slice_window`, `slice_range_window`,
`duplicate_window`, `typed_buffer_address`, `typed_direct_view_address`, plus
`file_channel_open_mode` and `heap_buffer_address`/`is_plausible_native_addr`
as controls) were extracted verbatim into `scratchpad/f37/geom.rs` and run
under `rustc --test`: **21 passed.** They pin geometry and an option algebra,
not a receiver — `MockNativeContext`'s name-to-slot fallback would make an
assertion about `arrayOffset()` a measurement of the mock's field table.

**All 20 mutants die.** Each is what a *specific wrong implementation*
answers, never a negated condition:

| mutant | tests killed |
|---|---|
| `typed_buffer_address` ignores the width (**= F26's `ByteBuffer` body reused verbatim**) | 3 |
| `typed_buffer_address` is a bare `16` (**= the shipped `alloc_typed_buffer` seed**) | 4 |
| `typed_direct_view_address` ignores the width | 1 |
| `typed_direct_view_address` ignores `rel_start` (**= the shipped s2 `$dup`'s "same address"**) | 1 |
| `typed_buffer_address` wraps instead of saturating | 1 |
| `slice_window` drops the source offset (offsets stop composing) | 1 |
| `slice_window` moves the offset only when something remains | 1 |
| `slice_range_window` folds in the source position | 2 |
| `slice_range_window` drops the source offset | 1 |
| **`duplicate_window` drops the offset (= F26-1 §3, in the typed twin)** | 2 |
| `duplicate_window` drops the mark | 1 |
| open mode: `read` is `names.is_empty()` rather than no-ACCESS-option | 1 |
| **open mode: WRITE implies TRUNCATE (the naive `open_write` mapping)** | 1 |
| open mode: WRITE implies CREATE (the naive `open_write` mapping) | 2 |
| open mode: APPEND does not imply write | 1 |
| open mode: only the READ+APPEND refusal is checked | 2 |
| open mode: only the APPEND+TRUNCATE refusal is checked | 2 |
| open mode: the two refusal messages are swapped | 1 |
| open mode: an unrecognised option is REFUSED (the afc policy applied here) | 1 |
| open mode: `CREATE_NEW` is not treated as `CREATE` | 1 |

**One mutant survived the first run, and the fixture set was wrong, not the
mutant.** "`slice_window` drops the source offset" passed all 20 tests, because
every `slice_window` fixture called it with `src_offset == 0`, where `pos` and
`src_offset + pos` are the same number. Added
`a_typed_slice_of_a_slice_adds_both_offsets` (a slice OF a slice, non-zero
first offset), after which it dies. A fixture set is only as strong as its
non-degenerate inputs, and the mutation run is what said so.

A second near-miss, recorded because it is the same species: the saturating-vs-
wrapping test originally used `usize::MAX`, for which `usize::MAX as i64` is
`-1` and the two operations AGREE — the test could not discriminate. It now
uses `(i64::MAX / 4)`, where the product genuinely overflows.

Twenty-one of these fixtures are also added to `native-io/src/lib.rs`'s
`buffer_bounds_tests` module (twelve `#[test]`s), so they run in CI rather than
only in a scratchpad.

All four owned files parse: `rustfmt --edition 2021 --emit stdout` on a COPY of
the whole `src` tree, exit 0. Line endings verified with
`tr -cd '\r' < f | wc -c` equal to the line count on every file, before and
after every edit — `grep -c $'\r'` matches every line in this shell and lies.

---

## 8. NOMINATIONS (exact text; not this lane's files)

### N1 — `native-builtins/src/lib.rs:5690` (W7-76 §8.2, the last of the five)

Carried forward from F26-1's N3, unchanged, because that file is being edited
by another session and this lane must not touch it.

OLD:
```rust
    ctx.set_field_by_name(this, "bigEndian", Value::Int(1));
    ctx.set_field_by_name(
        this,
        "nativeByteOrder",
        Value::Int(if cfg!(target_endian = "big") { 1 } else { 0 }),
    );
```
NEW:
```rust
    // CONVERGED (F37-1 §4, closing the LAST of W7-76 §8.2's five sites — the
    // other four now delegate to this same helper).
    cratonvm_native_io::seed_buffer_byte_order(ctx, this);
```

### N2 — `native-api/src/fd_table.rs`: no "open for write without creating"

`FileChannel.open(p, WRITE)` must open for write and **fail** if `p` does not
exist (MEASURED: `java.nio.file.NoSuchFileException`), and must **not**
truncate. `open_write` is `.create(true).truncate(!append)` and
`open_read_write` is `.read(true).write(true).create(create)` with no truncate
and no append — so the APPEND and TRUNCATE_EXISTING arms of §5.2 create a file
HotSpot would refuse to create.

Suggested addition, beside `open_read_write` (whose fd/entry idiom it copies
exactly):

```rust
    /// `FileChannel.open` / `Files.newByteChannel` with an explicit option
    /// set: `create`, `truncate` and `append` are all as ASKED, not implied.
    /// `open_write`'s `.create(true).truncate(!append)` is right for
    /// `FileOutputStream` and wrong for these, where `WRITE` alone must
    /// neither create nor truncate (MEASURED on jdk-25.0.3+9, F37-1 §5.2).
    pub fn open_write_options(
        &self,
        path: &str,
        create: bool,
        truncate: bool,
        append: bool,
    ) -> Result<FdId, io::Error> {
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(truncate && !append)
            .append(append)
            .open(path)?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileReadWrite(Mutex::new(file))));
        Ok(fd)
    }
```

`FileReadWrite` is chosen because `read_bytes`, `write_bytes` and `file_size`
all have an arm for it, so `native_fc_read` / `native_fc_write` /
`native_fc_size` keep working unchanged. With it, `native_fc_open`'s four-arm
`if` collapses to two:

```rust
    let opened = {
        let table = ctx.fd_table();
        if mode.write {
            table.open_write_options(&path_str, mode.create, mode.truncate, mode.append)
        } else {
            table.open_read(&path_str)
        }
    };
```

### N3 — `vm/src/runtime/interpreter/native_override.rs`: the typed families' view producers are not force-listed

`("slice", "()Ljava/nio/IntBuffer;")`, `("slice", "(II)…")`,
`("duplicate", "()…")` and `("asReadOnlyBuffer", "()…")` are **absent** from
the `java/nio/{Int,Long,Short,Float,Double}Buffer` arm at L3298-L3319, which
force-lists only the bulk `get`/`put`. `java/nio/CharBuffer` has no arm at all.

Consequence, and it is the reachability boundary of everything in §2: for a
receiver whose concrete class the real JDK loaded (`java.nio.HeapIntBuffer`,
`java.nio.ByteBufferAsIntBufferB`), the real bytecode wins and this lane's
natives never run. They run for VM-minted `java/nio/<T>Buffer`-stamped
receivers — which is what `alloc_typed_buffer` and `s2_view_buf_fn!` produce —
and for every receiver in synthetic-JDK mode.

**This is a DIAGNOSIS, not a proposed edit.** F26-1's N4 makes the same point
for `ByteBuffer.slice(II)`/`asReadOnlyBuffer()` and records the gate above that
arm ("Do NOT re-add a name here without also re-adding the eight probes").
Whoever takes it should decide the direction deliberately for both families at
once, and should note that with §2 landed the natives are now *closer* to the
JDK's behaviour than they were, which changes the risk calculus of forcing them
but does not settle it.

### N4 — `native-io` and `native-builtins` disagree about where a typed view's window lives

§2.3's third encoding. `servlet.rs`'s `s2_view_buf_fn!` parks the backing array
in `BB_SEGMENT_SLOT` and the byte start as `-(bs+1)` in the MARK slot;
`native-io`'s `bb_state` reads the array from that slot but takes the window
from `ByteBuffer.offset`, which such a receiver does not have — and reports
`elem` from the `byte[]`, so a `java/nio/IntBuffer` stamp reads bytes. Every
`native-io` accessor on an `allocate(n).asIntBuffer()` receiver is affected,
not only the view producers, and it predates this lane.

The two candidate directions are (a) teach `bb_state` the marker encoding and
the class-derived element kind, or (b) make `s2_view_buf_fn!` mint a receiver
`bb_state` already understands. Both are cross-crate layout agreements and
neither is safe without a build. This lane's contribution is to REFUSE to alias
over the ambiguity rather than to guess, and to name it.

### N5 — `regression-suite`: no fixture row asserts a read-only `put` throws

§3.1's defect had no assertion anywhere that could see it: the buffer fixtures
cover `array()` / `arrayOffset()` / `hasArray()` / `isReadOnly()` and the
contagion table (F21-1 N1's nineteen rows), and none of them writes. A row set
in `RJdkIntrinsics2.java`'s `bounds` section of the shape

```java
        t = null;
        try { ByteBuffer.allocate(8).asReadOnlyBuffer().put(0, (byte) 1); }
        catch (Throwable x) { t = x; }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)), …);
```

for each of {`ByteBuffer`, one typed heap family, one `ByteBufferAs<T>Buffer`
view} plus the two writable negative controls would have caught it, and would
catch a future registrar reordering that un-shadows a guardless body. Exact
class names, per F21-1 §5. Not written here because the denominator line must
be re-derived by running the fixture, which this lane can do for HotSpot but
not for CratonVM, and a half-measured denominator is worse than none.

---

## 9. Which checks FLIP and which merely become REACHABLE (predicted)

**Flip** — a currently-passing assertion would report a different value, or a
currently-failing one starts passing:

* every write through a read-only buffer now THROWS where it silently
  succeeded: `ByteBuffer` (all 13 `put*`/`compact` descriptors), the six typed
  families' `put`/`put(i,)`/`compact`, and the s2 views' bulk `put`;
* `<fam>Buffer.allocate(n).position(k).slice().arrayOffset()` → `k` where it
  was `0`; `.array()` is now the SOURCE array by identity;
* write-through visibility across `slice`/`slice(II)`/`duplicate` for all six
  typed families, in both directions;
* `allocateDirect(n).as<T>Buffer().slice().isDirect()` → `true` where it was
  `false`, and its content is shared;
* `allocate(n).asReadOnlyBuffer().as<T>Buffer().isReadOnly()` → `true`;
* `FileChannel.open(p, WRITE, CREATE)` yields a WRITABLE fd;
  `open(p, READ, APPEND)` and `open(p, APPEND, TRUNCATE_EXISTING)` now throw
  `IllegalArgumentException` where they silently opened for read;
* `FileChannel.size()` in **synthetic-jdk mode only** answers the file's length
  instead of `0` (in real-JDK mode `p57_fc_size` already did).

**Merely reachable** — no assertion changes, but a code path that could not run
before now can:

* the aliasing arm of `tb_derive_view` in synthetic-JDK mode. `hb` does not
  resolve there and `buf_try_set_heap_offset` cannot carry a non-zero offset,
  so every receiver refuses and takes the copy fallback — the same answers, a
  different branch producing them;
* `s2_typed_buffer_view_fns!`'s `$put`/`$put_abs`/`$compact` guards and
  `$slice`/`$slice2`/`$dup`/`$ro`'s flag writes: shadowed by `native-io` in
  every arm today (§1.3), so they change nothing until a registrar reorders —
  which is exactly when they must already be right;
* `s2_bb_set_read_only` on a bare 6-slot synthetic carrier is a by-NAME write
  with no field to land on, so in synthetic-JDK mode all of §3.4 is inert.

**Unchanged by construction:** `order()` on every derived buffer (the seed
moved in §4 but its value did not), the mark's survival through `duplicate()`
and its loss through `slice()`, the `hasArray`/`array`/`arrayOffset` three-way
table and its UOE-before-ROBE ordering, and `FileChannel.size()` in the two
real-JDK arms.

---

## 10. What this lane left undone

1. **The s2 heap-view encoding still copies** (§2.5), and its element-kind
   mismatch is a pre-existing wrong answer this lane refuses to alias over
   rather than fixes. N4.
2. **`ServiceLoader.toString()`** — untouched, per F26-1's "verify the slot
   first" (§5.3).
3. **`WatchEvent.count()`** (F26-1 §9.4 item 4) and **`native_bis_mark_supported`**
   (item 6) — untouched; both need a field or a receiver-set check that this
   lane cannot validate.
4. **`NonWritableChannelException` / `NonReadableChannelException`** are still
   unenforced on `FileChannel` (§5.2).
5. **No fixture row** asserts any of §3's throws. N5.
6. **Nothing here is type-checked.** Four files parse under `rustfmt`; the
   pure functions run under `rustc --test`. The `#[test]`s added to
   `native-io/src/lib.rs` have never been compiled in situ, and the two new
   trait-method call sites this lane introduces (`ctx.heap_kind_of` and
   `ctx.heap_element_type_of` inside `tb_alias_source`; `fd_table().file_size`
   and `open_read_write` in the FileChannel natives) are the most likely places
   for a signature surprise. All are used elsewhere in the same file with the
   same shapes, which is evidence and not proof.
