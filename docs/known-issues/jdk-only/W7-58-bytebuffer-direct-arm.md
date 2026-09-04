# W7-58 — `bb_state`'s missing direct-buffer arm, and the rest of a migration

**Status: source landed, UNVERIFIED against a VM.** Nothing here has been
built or run on CratonVM. The HotSpot oracle in §6 *was* measured — 156 checks
on Eclipse Adoptium jdk-25.0.3.9 — and the real JDK field layout in §2 was read
out of `lib/src.zip`, not assumed. Everything about what CratonVM does is read
from source and stated as such.

This is the lane W7-50-synthetic-jdk-strict-six.md opened and deliberately did
not take. Its §5 closed `RJdkDefineClass` by gating `register_nio_natives` off
the real-JDK arm, and recorded that the underlying defect survived:

> `bb_state`'s missing direct-buffer arm is a **real** defect in shared code,
> and it survives this fix — it is simply now unreachable from the real-JDK
> arm. […] A previous incident […] migrated some call sites and left roughly 55
> behind.

## 1. The count, and why ~55 was low

**48 textual production call sites; 78 after macro expansion.** Measured, not
estimated:

| | |
|---|---|
| `grep -c 'bb_state('` in `native-io/src/lib.rs` | 50 |
| minus the definition | 49 |
| minus one `#[cfg(test)]` unit test | **48 textual** |
| of which sit inside `macro_rules! tb_abstract_view_fns` | 6 |
| that macro's instantiations (Char/Int/Long/Float/Double/Short) | ×6 |
| **expanded production call sites** | 42 + 36 = **78** |

Every one of the 48 is in `native-io/src/lib.rs`. There is no second file.

The ~55 figure was read off a textual grep over a line range and never expanded
the macro — 6 written sites are 36 compiled ones. The range it quoted
(`:14537`–`:15352`) also stopped short of the ShortBuffer family, which runs to
`:15415` in the same numbering. Neither is a criticism of the estimate; it was
offered as an order of magnitude and it is one. It is a criticism of using it
as the size of the job: a macro is exactly where a "leave the rest for later"
decision goes wrong, because the leftovers are 6× what they look like.

**Sibling accessors in the same family**, for completeness — these are the
population `bb_state` sits in, and the reason the census is not simply "48":

| helper | callers | storage-aware before this lane? |
|---|---|---|
| `bb_storage_view` | 37 | yes — this is what the *first* migration produced |
| `bb_state` | 48 | **no** |
| `buf_read_position` / `buf_read_limit` / `buf_read_mark` | 15 | n/a — metadata only, correct already |
| `native_tb_array` | 12 registered triples (6 descriptors × 2 classes) | **no**, and not a `bb_state` caller — see §5.7 |

## 2. The layout, read rather than assumed

From `lib/src.zip`, `java.base/java/nio/Buffer.java` and `ByteBuffer.java` on
JDK 25, in declaration order, superclass first — which is how CratonVM numbers
instance fields (`native-builtins/src/servlet.rs`'s `BB_SEGMENT_SLOT = 5`
independently pins the same numbering):

| slot | field | declared in |
|---|---|---|
| 0 | `int mark = -1` | `Buffer` |
| 1 | `int position = 0` | `Buffer` |
| 2 | `int limit` | `Buffer` |
| 3 | `final int capacity` | `Buffer` |
| 4 | `long address` | `Buffer` |
| 5 | `final MemorySegment segment` | `Buffer` |
| 6 | `final byte[] hb` | `ByteBuffer` |
| 7 | `final int offset` | `ByteBuffer` |
| 8 | `boolean isReadOnly` | `ByteBuffer` |
| 9 | `boolean bigEndian` | `ByteBuffer` |
| 10 | `boolean nativeByteOrder` | `ByteBuffer` |
| 11, 12 | `Cleaner cleaner`, `Object att` | `DirectByteBuffer` |

CratonVM's own idea of the layout is `array(0) position(1) limit(2)
capacity(3) mark(4)`, `BB_NUM_FIELDS = 5`. Only slots 1 and 2 agree. Slot 0 is
the collision this record is about; slot 4 is the one `buf_set_mark` was
already repaired for on 2026-08-05 (it was stamping `mark` onto `address`).

Two corrections fall straight out of the table:

* `HEADER_SIZE` has been 16 since 2026-08-07, and `ARRAY_BYTE_BASE_OFFSET` is
  16 here. `alloc_byte_buffer` seeds `address = Long(16)` into every **heap**
  buffer it mints, mirroring the real `HeapByteBuffer` constructor. That is an
  array-relative offset, not a pointer. See §4.
* `bb_state`'s comment said its slot-5 fallback was "the real-JDK
  HeapByteBuffer slot (`hb` @ 5)". On JDK 25 `hb` is **6** and 5 is
  `Buffer.segment`. The probe is kept — `segment` is the only Object-typed
  field `Buffer` declares, which is precisely why `native-builtins`'
  typed-buffer views stash their backing array there — but it is kept for that
  reason, not the one written down.

## 3. The defect

`bb_state` resolved a backing array as `hb`-by-name → slot 5 → slot 0, and had
no fourth arm. A buffer with no backing array therefore reached slot 0, read
`Buffer.mark = -1`, and raised

```
internal error: ByteBuffer missing backing array (field 0 returned Int(-1) for object ObjectRef)
```

The value in the message is the tell and was there all along: `-1` is not a
corrupted array reference, it is a correctly-read `int` field that means
"unmarked". The species is a CratonVM slot index applied to a real JDK object.

Two aggravating facts, both structural rather than incidental:

1. **The refusal reached callers that did not want the array.**
   `native_bb_remaining` is `limit - position`. It called `bb_state`, bound the
   array to `_`, and inherited a refusal about storage it never touches.
   `remaining()` is declared on `java.nio.Buffer`, above any notion of storage;
   it must answer for heap, direct and read-only receivers alike. Same for
   `hasRemaining()` and the typed `toString()`.
2. **A heap buffer works through both paths**, which is why a partial
   migration survived. The first migration
   (spring-bytebuffer-backing-storage-FIXED.md) moved the byte-oriented half to
   `bb_storage_view`, which has both arms; the half left behind is only
   distinguishable when a non-heap buffer arrives.

## 4. What changed

All in `native-io/src/lib.rs`.

**`bb_state` is now storage-aware and returns `TbView`, not a tuple.** The
type change is the point. This is the second migration of this family; the
first left stragglers that were invisible until a direct buffer reached them.
A changed return type means any site left behind fails the **build**. That is
the ratchet — the same shape as
`a-measured-population-closes-by-becoming-a-ratchet`.

| piece | what it does |
|---|---|
| `buf_read_capacity` | the name-then-slot capacity read, which existed inline in three places |
| `buf_metadata` | `(pos, lim, cap)` with no storage requirement — for the callers that never wanted the array |
| `bb_resolve_heap_array` / `bb_resolve_heap_offset` / `bb_resolve_direct_address` | one copy each, shared by `bb_state` and `bb_storage_view`, which had drifted apart (different error text, and only one had a direct arm) |
| `is_plausible_native_addr` | `addr >= 0x1_0000` — see below |
| `TbView` + `tb_read_elem` / `tb_write_elem` | element-indexed access over both storage kinds; a direct block is byte-addressed, so the index is scaled by the element width and the bytes decoded in the receiver's order |
| `tb_direct_element_kind` / `tb_receiver_is_big_endian` | how a **direct** receiver's element width and order are found |

**The address screen is not padding.** `bb_storage_view` accepted any non-zero
`address`. `alloc_byte_buffer` seeds `address = Long(16)` into every heap
buffer. So a heap buffer whose array failed to resolve — exactly the condition
that reaches the direct arm — handed `16` to `copy_from_native_memory` as a
pointer. `addr=0x10` was 51 of the 53 crashes in the 2026-08-10 three-GC H2
sweep. `native-builtins/src/servlet.rs`'s `is_plausible_native_addr` is the
same guard for the same reason; this family did not have it. This is the one
behaviour change to already-migrated code in this lane, and it converts a
SIGSEGV into a visible error.

**On `tb_direct_element_kind` and the never-bind-by-NAME rule.** A heap
receiver is asked `heap_element_type_of`, i.e. the backing array's own truth —
no name involved. Only a **direct** receiver, which has no array to ask, falls
back to the class name, because that is where the JDK itself puts the
distinction: `DirectIntBufferU`, `ByteBufferAsIntBufferL`, `HeapIntBuffer`.
Same for the byte order, which for a view buffer has no field at all and is
encoded solely in the `…B` / `…L` class suffix — the already-registered
`order()` native for these classes reads exactly that, and the two now share
one helper so they cannot disagree. This is decoding an authored class
distinction, not selecting which native runs from a name; the prohibited shape
is `format!("get{cap}")` and a hard-coded descriptor, as in
`W7-50-synthetic-jdk-strict-six.md` §7.

## 5. The site census

`this` = caller-supplied receiver; `fresh` = the buffer `alloc_byte_buffer` /
`alloc_typed_buffer` minted one or two lines above, which always installs a
backing array in both `hb` and slot 0.

### 5.1 ByteBuffer family (4 textual, 4 expanded)

| site | receiver | class | action |
|---|---|---|---|
| `native_bb_wrap` | fresh | heap-only | routed through `tb_write_elem`; cannot become a straggler |
| `native_bb_wrap_range` | fresh | heap-only | same |
| `native_bb_remaining` | `this` | **direct-reachable** | **migrated to `buf_metadata`** — this is `RJdkDefineClass` |
| `native_bb_has_remaining` | `this` | **direct-reachable** | **migrated to `buf_metadata`** |

### 5.2 `tb_abstract_view_fns!` macro (6 textual, 36 expanded)

Instantiated for CharBuffer, IntBuffer, LongBuffer, FloatBuffer, DoubleBuffer,
ShortBuffer.

| site | receiver | class | action |
|---|---|---|---|
| `$slice_fn` ×6 | `this` | **direct-reachable** | migrated |
| `$slice_fn` ×6 | fresh `new_buf` | heap-only | routed through `tb_write_elem` |
| `$slice2_fn` ×6 | `this` | **direct-reachable** | migrated |
| `$slice2_fn` ×6 | fresh `new_buf` | heap-only | routed through `tb_write_elem` |
| `$dup_fn` ×6 | `this` | **direct-reachable** | migrated |
| `$dup_fn` ×6 | fresh `new_buf` | heap-only | routed through `tb_write_elem` |

`$ro_fn` delegates to `$dup_fn` and `$order_fn` touches no storage; neither is
a call site.

### 5.3 CharBuffer family (11 textual)

| site | receiver | class | action |
|---|---|---|---|
| `native_cb_wrap` | fresh | heap-only | routed through `tb_write_elem` |
| `native_cb_wrap_charseq` | fresh | heap-only | routed through `tb_write_elem` |
| `native_cb_wrap_charseq_range` | fresh | heap-only | routed through `tb_write_elem` |
| `native_cb_get` | `this` | **direct-reachable** | migrated |
| `native_cb_get_abs` | `this` | **direct-reachable** | migrated |
| `native_cb_put` | `this` | **direct-reachable** | migrated |
| `native_cb_put_abs` | `this` | **direct-reachable** | migrated |
| `native_cb_put_string` | `this` | **direct-reachable** | migrated |
| `native_cb_to_string` | `this` | **direct-reachable** | migrated |
| `native_cb_char_at` | `this` | **direct-reachable** | migrated |
| `native_cb_compact` | `this` | **direct-reachable** | migrated |

### 5.4 Shared typed helpers (2 textual)

| site | receiver | class | action |
|---|---|---|---|
| `native_tb_to_string` | `this` | **direct-reachable** | **migrated to `buf_metadata`** — never needed storage |
| `native_tb_compact` | `this` | **direct-reachable** | migrated |

### 5.5 Int / Long / Float / Double / Short families (25 textual)

Identical shape in all five. Per family:

| site | receiver | class | action |
|---|---|---|---|
| `native_{i,l,f,d,s}b_wrap` | fresh | heap-only | routed through `tb_write_elem` |
| `native_tb_get_<t>` | `this` | **direct-reachable** | migrated |
| `native_tb_get_<t>_abs` | `this` | **direct-reachable** | migrated |
| `native_tb_put_<t>` | `this` | **direct-reachable** | migrated |
| `native_tb_put_<t>_abs` | `this` | **direct-reachable** | migrated |

### 5.6 Totals

| class | expanded sites |
|---|---|
| **direct-reachable, migrated** | **50** |
| structurally heap-only, left in place (and said per site above) | 28 |
| already migrated before this lane | 0 — `bb_storage_view`'s 37 callers are the *first* migration, a disjoint population |
| **total** | **78** |

Nothing is left unmigrated in the sense of "still assumes heap". The 28
heap-only sites are left *as heap-only*, with the reason stated per row: their
receiver is a buffer the same function allocated two lines earlier. They still
go through `bb_state` + `tb_write_elem`, so if that ever stops being true they
fail with the rest rather than silently.

### 5.7 One sibling that is not a `bb_state` caller, fixed anyway

`native_tb_array` — registered as `array()[C` / `[I` / `[J` / `[F` / `[D` /
`[S` for all six typed families — read **raw slot 0** and returned it. Same
species, but reachable from Java rather than only through an internal error:
on a real-layout receiver it returned `Int(-1)` (`Buffer.mark`) where its
descriptor promises an array, and on a direct view it invented a backing array
for a buffer that has none. It now resolves properly and throws
`UnsupportedOperationException` for a direct receiver, which is what
`Buffer.array()` specifies. The storage-less case keeps its historic benign
null: `native-builtins`' typed views can legitimately arrive with neither
array nor address, and turning that into a throw is a change this lane has no
vector to measure.

## 6. The probe

`probes/DirectByteBufferStateProbe.java`, with
`probes/DirectByteBufferStateProbe.expected.txt`. **156 checks, all measured on
HotSpot** (Eclipse Adoptium jdk-25.0.3.9), none guessed.

Structure: the same battery over `ByteBuffer.allocateDirect(16)` and
`ByteBuffer.allocate(16)`. **The heap arm is the control** — if it diverges
too, the defect is wider than the missing direct arm, and that is the more
important finding.

Every assertion is an exact value. Deliberately avoided: `remaining() >= 0`,
which passes against `-1`-derived garbage in most arrangements, and
"did-not-throw" checks. Where a throw is expected the probe records the
*fully-qualified exception name*, and when nothing is thrown it records
`NO-THROW:<value>` — so a native that answers a direct `array()` with a
fabricated array shows what it returned rather than just failing an assertion.

Two things the probe had to get right that are easy to get wrong:

* `ReadOnlyBufferException extends UnsupportedOperationException`. Catching the
  wider one first is a compile error; reordered by hand without noticing, it
  would relabel every read-only refusal as an unsupported-operation one. The
  measured JDK answers differ by arm: a direct read-only view's `array()`
  throws `UnsupportedOperationException`, a heap read-only view's throws
  `ReadOnlyBufferException`.
* `hasArray()` is `hb != null && !isReadOnly`, so **both** read-only arms
  answer `false` — including the heap one, whose array exists.

### What the probe is expected to show, and what this lane does not fix

Predictions from source, not measurements. Only the first row is this lane's.

| probe lines | before | after this lane |
|---|---|---|
| `direct.remaining`, `direct.hasRemaining`, `direct.afterFill.*`, `direct.setPosition.remaining`, and every later line on the direct arm (the internal error aborts the arm) | internal error | green |
| `*.slice.aliasesParent` | red on **both** arms | still red — `native_bb_slice` copies rather than aliases. A genuine defect, a different one, and not `bb_state`'s |
| `*.readOnly.isReadOnly` | red on both arms | still red — `native_bb_is_read_only` is a hard-coded `Ok(Some(Value::Int(0)))` |
| `*.getIntLE`, `*.putIntLE.*` | red | still red — the hard-coded `to_be_bytes` / `from_be_bytes` family, which is `RJdkNio`'s half of W7-50 §5 and is named there |

Those three are recorded, not fixed. Each is a distinct defect in the same
registrar, none is reachable through `bb_state`, and folding them in would make
this change unattributable.

## 7. Where this is reachable — read this before running the probe

After W7-50, `register_nio_natives` is called from exactly one place
(`native-io/src/lib.rs`, inside `register_io_natives`) under

```rust
#[cfg(feature = "synthetic-jdk")]
if !registry.drops_real_layout_synthetic() {
```

so it runs **only** in a `--features synthetic-jdk` binary running
`--synthetic-jdk`. Every one of the 78 sites is registered by that one
registrar and no other; a **real** `java.nio.DirectByteBuffer` can no longer
reach any of them.

What reaches them is the synthetic direct buffer minted by
`native-builtins/src/servlet.rs`'s `s2_bb_alloc_direct` (NEW-17): slot 0 null,
slot 4 carrying a real off-heap address from the VM's `NativeMemoryTable`.
`bb_resolve_direct_address`'s slot-4 fallback finds it and
`bb_resolve_heap_array` correctly finds nothing, so the new arm fires on
exactly that shape. **The probe must therefore be run
`--features synthetic-jdk` + `--synthetic-jdk`.** Run under the default
real-JDK mode it exercises real JDK bytecode and passes without touching any
of this.

### Registration is last-write-wins, and this family WINS

`vm/src/vm/vm_init.rs:1580`–`:1581` calls `register_builtins` and *then*
`register_io_natives`. So for every triple both register — `remaining`,
`hasRemaining`, `array`, `get`, `put`, `compact`, `slice`, `duplicate`,
`toString` on `ByteBuffer` and on all six typed classes — native-io's copy
overwrites `native-builtins`' and is the one that runs. Checked, not assumed:

| triple | registrars | winner |
|---|---|---|
| `java/nio/ByteBuffer.remaining()I` | `servlet.rs:5492` (s2), `lib.rs:8028` (nio) | **nio** |
| `java/nio/IntBuffer.remaining()I` | `servlet.rs:5941` (s2), `lib.rs:8243` (nio) | **nio** |
| `java/nio/CharBuffer.remaining()I` | `charset_buffers.rs:1617` (p62), `lib.rs:8189` (nio) | **nio** |
| `java/nio/IntBuffer.array()[I` | `servlet.rs:5971` (s2), `lib.rs:8252` (nio) | **nio** |
| `java/nio/CharBuffer.array()[C` | `charset_buffers.rs:1712` (p62), `lib.rs:8206` (nio) | **nio** |

So patching these is not inert. The uncomfortable half of that finding: the
losers are *better* code. `native-builtins`' s2 family is fully storage-aware
(`s2_bb_storage`, `s2_bb_direct_addr`, `is_plausible_native_addr`) and its
`array()` already threw `UnsupportedOperationException` for a direct receiver.
The winner was the heap-only one. **This lane brings the winner up to the
loser; it does not resolve which should win**, and that question — whether
`register_nio_natives` should still overwrite the s2 family in synthetic mode
at all — is a larger one that belongs with a vector that runs in synthetic
mode.

### `NativeKind` ambience

`register_nio_natives` saves `registry.current_category()` on entry, sets
`Bridge`, and restores on exit — so it is ambient-neutral for everything
registered after it. No registration was added, moved or removed in this lane,
so no block boundary changed and no `NativeKind` moved.

## 8. `report_layout_alias` — what it can and cannot see here

`native-builtins/src/util_concurrent_ext.rs:868`, reached only from
`try_alloc_concurrent_synthetic`, enabled by `CRATONVM_DBG_LAYOUT_ALIAS`.

**It cannot see this defect, in either direction, and the reason is
structural rather than a gap to be closed:**

1. **It is an ALLOCATION instrument, not a READ one.** It fires inside the
   fabrication funnel, comparing the slot count a native *asked for* against
   `class_num_total_fields`. The defect here is `ctx.get_field(this, 0)`
   applied to an object this native did **not** allocate. No allocation
   happens on that path, so nothing is measured.
2. **It compares COUNTS, never slot identity.** Its input is one integer. It
   has no way to express "slot 0 on `java.nio.Buffer` is `mark`, not `hb`" —
   which is the whole defect. A native reading the wrong field of a
   correctly-sized object is outside its vocabulary.
3. **Both natives that allocate buffers in `native-io` bypass the funnel
   entirely.** `alloc_byte_buffer` and `alloc_typed_buffer` call
   `ctx.alloc_object(cid, n)` directly. They never appear in this census at
   all — not as `under`, not as `over`. Note `alloc_byte_buffer` asks for
   `BB_NUM_FIELDS = 5` on `java/nio/ByteBuffer` while `native-builtins` asks
   for 6 on the same class; that disagreement is invisible for the same reason.
4. **The `over`-direction widening of 2026-08-11 does not help.** That fixed a
   real half-census — the instrument had been blind to over-allocation, which
   is the corruption-shaped direction — but both directions are still about
   counts at allocation time.
5. **Its documented discriminator also misses.** The function's own comment
   proposes intersecting this census with `cratonvm::gc::guard`'s
   out-of-bounds reads: "a class appearing in BOTH has a live defect". Slot 0
   **exists** on every ByteBuffer, so the read is perfectly in bounds and the
   guard never fires. An in-bounds read of the *wrong* field is invisible to
   both halves of that pair.

**What it does report nearby**, so the signal is not mistaken for this defect:
`s2_bb_alloc` asks `try_alloc_concurrent_synthetic` for **6** fields on
`java/nio/HeapByteBuffer`, whose real JDK 25 layout declares **11** (§2). Under
`CRATONVM_DBG_LAYOUT_ALIAS` that is one `direction=under` row. It is a real row
and a different problem — an under-allocation the funnel clamps up — and it
would still be there with `bb_state` perfectly correct.

The instrument that *would* have caught this is the probe in §6: an exact
comparison against HotSpot on a receiver of the kind the code never saw.

## 9. Compatible mode (`--real-jdk`)

Contractually frozen except for genuine HotSpot-parity bug fixes. Per change:

| change | touches Compatible? | why justified |
|---|---|---|
| `bb_state` direct arm; 50 site migrations; `TbView`/`tb_read_elem`/`tb_write_elem`; `buf_metadata`; `native_tb_array` | **No.** Every affected registration is inside `register_nio_natives`, which since W7-50 runs only in a synthetic-jdk build in synthetic mode. In a default build the whole registrar is `#[cfg]`-compiled out. | n/a — unreachable from Compatible |
| `is_plausible_native_addr` in `bb_storage_view` | Only through the same registrar, so also not reachable from Compatible. | Would be justified regardless: dereferencing `Buffer.address = 16` as a pointer is an immediate SIGSEGV, not a wrong answer, and refusing it is strictly closer to HotSpot than crashing |
| `buf_read_capacity` / the three shared resolvers | No behaviour change anywhere — the same reads, in one copy instead of two. | n/a |

No `CRATONVM_*` env var was added, so the four-file flag surface
(`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
`docs/flag-tokens.md`, `docs/config/flag-inventory.md`) is untouched.
`vm/src/vm/tests.rs` was not touched.

## 10. Tests

No existing test was weakened. `bb_get_bulk_valid_copies_bytes` was rewritten
to the new API and asserts the same thing.

Six added, each pinning something that could not previously fail:

| test | what it pins |
|---|---|
| `bb_state_resolves_a_direct_buffer_through_address` | the exact receiver that used to fail, with slot 0 pinned **as `-1`** so it cannot pass by that slot happening to hold something else |
| `remaining_needs_no_backing_storage` | asserts `bb_state` still **errors** on that receiver first — otherwise the two assertions below it would prove nothing (the vacuous shape: a probe that cannot fail) |
| `direct_arm_refuses_an_array_base_offset_as_a_pointer` | `address = 16` is refused by both `bb_state` and `bb_storage_view` |
| `direct_typed_view_scales_by_element_width_and_honours_order` | element **1** of a direct `IntBuffer` is byte 4, not byte 1; `…B` vs `…L` decode differently; a write round-trips |
| `direct_char_is_unsigned_and_short_is_signed` | the two share a width, so sign extension is the only thing separating them, and getting it wrong is invisible until the high bit is set |
| `typed_array_accessor_refuses_a_direct_receiver_and_never_returns_mark` | §5.7 |

`bb_get_bulk_reads_real_heap_layout_slot_hb` is left as it is. Its name
records the belief corrected in §2 — its slot 5 is `segment`, not `hb` — but
the assertion is still a correct statement about what the slot-5 probe does,
and renaming it would lose the trail.

## 11. Not resolved without a build

* **Everything about CratonVM's behaviour.** No arm of this was run. The
  probe's oracle is measured; its CratonVM result is not.
* **Whether the 28 heap-only sites are all genuinely unreachable with a direct
  receiver.** Each was classified by reading its own function: the receiver is
  a buffer that function allocated. That is a strong local argument and a
  source-only one.
* **`native_bb_slice`'s copy-instead-of-alias, `native_bb_is_read_only`'s
  constant `0`, and the hard-coded-endianness typed accessors** (§6). Three
  live defects in the same registrar, named and left.
* **Whether `register_nio_natives` should overwrite the s2 family in synthetic
  mode at all** (§7). The losers are the better code. That is the question this
  lane surfaced and did not answer.

Two pre-existing hazards seen while migrating, neither introduced nor fixed
here, both recorded so the next reader does not have to re-find them:

* **§6's probe now has a partial scheduled home — and asking which of its rows a
  fixture could reach found a fifth defect in the same family.** See §12.

* **The typed `slice`/`slice(int,int)`/`duplicate` macro holds a resolved
  `ObjectRef` across an allocation.** `bb_state` is called on `this`, then
  `alloc_typed_buffer` runs — a collection point — and the earlier view's
  array reference is used afterwards. The old tuple form had exactly the same
  shape (`let (arr, …) = bb_state(…)` … `alloc_typed_buffer` … `arr`), so this
  is unchanged, but it is the `pin_native_root` / `read_native_pin` idiom that
  `s2_bb_alloc` uses two files over and this family does not.
* **A `native-builtins` typed VIEW carries its byte start in slot 4 as
  `-(offset + 1)`**, which `s2_bb_int_byte_off` decodes. `bb_resolve_heap_offset`
  reads `offset`-by-name and slot 6, so a native-io typed accessor applied to
  such a view indexes from element 0 of the shared array rather than from the
  view's window. Also unchanged — the old `bb_state` returned the array with no
  offset at all — but it means the two families disagree about where a view
  starts, and native-io's is the registration that wins (§7).

---

> **VERIFIED AGAINST A BINARY 2026-09-03. `bb_state`'s missing direct-buffer arm
> is REAL and still live on both shipping arms.** This record's status was
> "Nothing here has been built or run on CratonVM"; the CratonVM column exists
> now, from `probes/DirectByteBufferStateProbe.java` against Temurin
> 25.0.3+9-LTS on the same host.
>
> ```text
>                   checks ok   BAD   probe reached the end?
> HotSpot 25            282       0   yes -- PROBE PASS
> compatible            262       2   NO  -- dies at line 492
> --jdk-only            262       2   NO  -- dies at line 492
> --synthetic-jdk       255       9   NO  -- dies at line 492
> ```
>
> **The two BAD rows on the shipping arms are this record's subject**, and they
> are the defect W7-50 handed over — the one that "survives this fix ... simply
> now unreachable from the real-JDK arm":
>
> ```text
> seg.heap.isDirect   expected false   got true
> seg.heap.hasArray   expected true    got false
> ```
>
> `MemorySegment.ofArray(byte[]).asByteBuffer()` produces a buffer that reports
> itself DIRECT and array-less. A heap-backed segment took the direct arm.
>
> **And it then kills the probe.** Because `hasArray` is false, `hb.array()` on
> the next line throws `UnsupportedOperationException` out of `segmentBuffer`
> (`DirectByteBufferStateProbe.java:492`) and the run stops there. The ~20
> remaining rows — `seg.heapSlice.*`, `seg.heapRO.*`, `seg.intArray.*` — are
> **UNTESTED, not passing.** A naive line-diff reports "23 differing lines" and
> reads as a broad divergence; 2 are wrong answers and the rest are a truncation.
> Counted as a diff it also flatters the VM: the aliasing and read-only rows
> most likely to catch a fabricated buffer are exactly the ones never reached.
>
> **The `getIntLE` / `putIntLE` residuals this record documents are
> `--synthetic-jdk`-ONLY.** They do not reproduce in Compatible or `--jdk-only`:
>
> ```text
> heap.getIntLE            expected 824845084   got 472066609    (synthetic only)
> heap.putIntLE.byte4/7    expected 4 / 1       got 1 / 4        (synthetic only)
> heap.ord.asIntBuffer.*   expected BIG_ENDIAN  got LITTLE_ENDIAN (synthetic only)
> ```
>
> That is new information the record could not have: it names them as residuals
> without a mode, and they are absent from both shipping arms.
>
> **What this does NOT verify.** §1's census — 48 textual call sites, 78 after
> macro expansion — is a source count and was not re-counted; this note verifies
> BEHAVIOUR only, and only for the sites this probe reaches. Nothing here says
> which of the 78 sites produces the `isDirect` answer: the defect is confirmed
> and NOT localised. The rows after line 492 are unmeasured in every CratonVM
> mode and no claim is made about them in either direction.

> **FIXED 2026-09-04.** The defect this record named — `bb_state`'s missing
> direct-buffer arm, surviving as *"a real defect in shared code"* — is closed,
> and `probes/DirectByteBufferStateProbe.java` now runs to the end:
>
> ```text
>                        checks   BAD   reached the end?
> HotSpot 25               282      0   yes
> CratonVM compatible      282      0   yes -- PROBE PASS
> CratonVM --jdk-only      282      0   yes -- PROBE PASS
> ```
>
> Before: 262 checks, 2 BAD, and the run DIED at line 492, leaving ~20 rows
> untested.
>
> **What it actually was.** `MemorySegment.asByteBuffer()` minted a
> `DirectByteBuffer` unconditionally — the registration comment said so
> outright, *"asByteBuffer() → a direct ByteBuffer over the segment's own
> memory"*, stating the defect as if it were the design. A heap segment has no
> native address, so `ofArray(byte[16]).asByteBuffer()` answered
> `isDirect() == true` and `hasArray() == false`, and `hb.array()` on the next
> probe line threw `UnsupportedOperationException` out of the run.
>
> **The fix** mirrors `HeapMemorySegmentImpl.makeByteBuffer()`: a heap-backed
> segment becomes a HEAP buffer over its own backing array, built as
> `ByteBuffer.wrap(base, start, size).slice()` — `wrap` sets position/limit and
> `slice` turns the remaining window into capacity while folding the position
> into `arrayOffset()`, which is byte-for-byte the shape
> `newHeapByteBuffer(base, start, size, seg)` produces. Two natives that are
> registered in every mode were chosen deliberately over `slice(II)`, which this
> VM registers for `CharBuffer`/`IntBuffer`/`LongBuffer`/`FloatBuffer` and **not**
> for `ByteBuffer` — a gap this note records and does not close.
>
> A non-`byte[]` base is refused by name, as the oracle does
> (`ofArray(int[8]).asByteBuffer()` is `UnsupportedOperationException` on
> HotSpot 25), including when the shape check declines to build a view at all —
> without that second arm an `int[]` segment fell through to the direct path and
> answered with a `DirectByteBuffer` over memory it does not own.
>
> **What is NOT claimed.** §1's census — 48 textual call sites, 78 after macro
> expansion — was not re-counted, and this fix touches the `asByteBuffer`
> producer rather than the `bb_state` consumer, so a site that mints a segment
> buffer some other way is unaffected and untested. The `getIntLE`/`putIntLE`
> residuals remain `--synthetic-jdk`-only and are not addressed here.

## 12. §6's probe, scheduled — the reachable half (2026-08-12)

§7 is right that "the probe must be run `--features synthetic-jdk` +
`--synthetic-jdk`" **for this record's own changes**, and that is exactly why
this record's 78 sites are the part a scheduled fixture cannot reach: the suite
runs Compatible mode, where `register_nio_natives` is not even compiled in a
default build.

But §6's probe is not only about `bb_state`. It is a HotSpot oracle for the
`java.nio.ByteBuffer` **contract**, and in Compatible mode that contract is
answered by the s2 family, which W7-76 §2 shows wins there. So the probe's rows
split three ways, and the split is the useful output of asking the question:

| probe rows | who answers in Compatible mode | schedulable? |
|---|---|---|
| the two `arm()` batteries — position/limit/remaining/bounds/slice/duplicate/read-only/compact | s2 | **already covered** — `RDirectBufferElem` groups 1–5 assert the same contract on the same two receivers and are green today |
| `*.ord.*` (order propagation) and `*.win.*` (`array`/`arrayOffset` on a window) | s2 | **yes** — now groups 6 and 7 of `RDirectBufferElem`. W7-76 §10 and W7-83 §8.1 are the two records those groups discharge |
| `seg.*` | nothing — `MemorySegment.asByteBuffer()` has no registration in either crate | **no**, and W7-83 §8 has the grep |
| this record's own 78 `bb_state` sites | `native-io`, synthetic mode only | **no**, and §7 already says so |

**The fifth defect.** §6's prediction table names four things it does not fix
(`slice.aliasesParent`, `readOnly.isReadOnly`, the hard-coded `to_be_bytes`
family, and the direct arm it does fix). Writing the `*.ord.*` assertions found a
fifth, in `native-builtins` rather than `native-io` and therefore live in the
shipping mode: **all four s2 derivations propagate the parent's byte order, and
HotSpot resets it.** It is recorded in full at W7-76 §10, because that is the
record whose §4.3 measured the oracle, and it is the clearest instance yet of
this campaign's own rule — §4.3 measured HotSpot, drew the right conclusion about
the *task's* premise, and never turned the same question on the code.

**What the s2 family gets right, checked while doing this**, so the next reader
does not re-derive it: `array()`'s three-arm match already produces HotSpot's
measured heap/direct read-only split exactly (`Some(arr)` + read-only →
`ReadOnlyBufferException`; `None` + a plausible direct address →
`UnsupportedOperationException`), and `hasArray()` is already
`s2_bb_arr(..).is_some() && !s2_bb_is_read_only(..)`, i.e. the JDK's
`hb != null && !isReadOnly`. Both are asserted by group 7 as the over-correction
arm rather than as findings.

---

## 13. Verification pass 2026-08-12 (lane A16)

**Nothing was built or run in this pass.** Source read against today's
worktree, with today's anchors.

### 13.1 The landed change is intact

| claim | today | verdict |
|---|---|---|
| `bb_state` returns `TbView`, heap arm → direct arm → refusal | `native-io/src/lib.rs:7844`–`:7883` | present; the refusal message now names every probe it tried, which is strictly better than the `Int(-1)` string §3 quotes |
| `buf_metadata`, no storage requirement | `:7724` | present |
| `is_plausible_native_addr` | `:8045`, `v >= 0x1_0000` | present |
| `bb_resolve_direct_address` (`address`-by-name → slot 4, screened) | `:8050`–`:8082` | present |
| `TbView` | `:8159` | present |
| §5.7 `native_tb_array` refuses a direct receiver with `UnsupportedOperationException` and keeps the storage-less null | `:16018`–`:16030` | present |
| §7's gating | `:6874`–`:6877`, `#[cfg(feature = "synthetic-jdk")] if !registry.drops_real_layout_synthetic()` | **unchanged** — the 78 sites are still reachable only in a `--features synthetic-jdk` binary running `--synthetic-jdk` |

The three defects §6 named and left are all still exactly as named:
`native_bb_is_read_only` is still `Ok(Some(Value::Int(0)))` (`:9757`–`:9759`),
and `native_bb_slice` still copies rather than aliases (`:9801`+). Add one this
record did not name: `native_bb_duplicate` (`:9761`+) falls back to a **byte
copy** for a receiver whose array does not resolve (`:9789`–`:9796`), so
`duplicate()` on a direct buffer does not alias either. Same defect family as
`slice`, same registrar, still not `bb_state`'s.

### 13.2 §2 and §4 have been amended by a later lane — read W7-83 with this

This record's §2 keeps the slot-5 probe with the justification that `segment` is
the only Object-typed field `Buffer` declares, and §4's `bb_resolve_heap_array`
resolves `hb` → slot 5 → slot 0. **W7-83-segment-as-backing-array.md measured
the "harmless for a real buffer" half and falsified it**: `Buffer.segment` is
null on `allocate` and `allocateDirect` receivers but is a live
`jdk.internal.foreign.NativeMemorySegmentImpl` on an
`Arena…allocate(n).asByteBuffer()` one, which the slot-5 arm was handing to
`heap_element_type_of` / `get_array_element` as a backing array. The arm now
carries a **kind screen** — see the doc comment at `native-io/src/lib.rs:7903`+
and the new unit test `bb_state_refuses_a_memory_segment_at_the_segment_slot`
(`:24477`). §2's bullet about the old comment being "wrong twice over" is right
and is now in the source; what it did not anticipate is that the *value's kind*,
not the slot index or the field name, was the third way to be wrong.

**§8 is correspondingly half-superseded.** §8 concludes that the instrument
which would have caught this is the §6 probe, because `report_layout_alias` is
an allocation instrument that compares counts. A **read-side** alias instrument
now exists — `native-api/src/read_alias.rs`, with
`bb_state_reading_slot_0_of_a_real_direct_byte_buffer_as_hb_is_flagged`
(`:711`) — and `bb_resolve_direct_address` calls `read_alias::observe_read` on
its slot-4 fallback under `layout_alias::enabled()` (`native-io/src/lib.rs:8069`).
§8's five numbered reasons are still all true **of `report_layout_alias`**; they
are no longer true of the tree.

### 13.3 A residual this record does not carry: the address screen does not see a TAGGED handle

`is_plausible_native_addr` is `v >= 0x1_0000`, and its doc comment justifies
exactly one thing — refusing `ARRAY_BYTE_BASE_OFFSET`-shaped small values, which
is what §4 claims and what the 2026-08-10 `addr=0x10` crash population needed.

It does **not** discriminate a real off-heap address from an **arena-tagged
handle**. `Unsafe.allocateMemory` in this VM returns a tagged handle, not a raw
address (`0x4000_0010_…`-shaped), and such a value passes `>= 0x1_0000`
trivially. §7 states that what actually reaches the direct arm today is
`servlet.rs`'s `s2_bb_alloc_direct`, which seeds a real address from the VM's
`NativeMemoryTable` — so this is not a live crash, it is a **missing screen on
an arm whose sole safety argument is the identity of its one producer**. If any
other producer ever seeds `address` from `Unsafe.allocateMemory`, `tb_read_elem`
dereferences a handle and the failure is a SIGSEGV inside the copy, not an
error. Source-only, not measured; recorded because §11 already lists "whether
the 28 heap-only sites are genuinely unreachable" as the same species of
producer-identity argument.

### 13.4 §12's scheduling is real

`RDirectBufferElem` is in `CORE_CLASSES` (`regression-suite/run.sh:106`), so it
runs on a default `run.sh` invocation in both modes, and the file carries the
group 6/7 material §12 describes (`RDirectBufferElem.java:83` names "the
measured heap/direct split in group 7"; the heap/direct/read-only sections are
at `:391`, `:411`, `:432`).

### 13.5 The passing direct-ByteBuffer probe does NOT contradict this record

A 33-probe reachability screen run today on the current binary reports direct
`ByteBuffer.allocateDirect` + `putInt`/`getInt` **passing** under `--jdk-only`
(and on the HotSpot oracle). That looks like a refutation of §3 and is not one,
for the reason §7 already gives: the screen ran a **default build** in
`--jdk-only`, where `register_nio_natives` is not compiled at all, so
`allocateDirect`/`putInt`/`getInt` were answered by real JDK bytecode and the
`native-builtins` s2 family — none of the 78 sites this record migrated. It is a
**control for §12's three-way split**, and a useful one: it says the contract
those rows assert is green in the mode the suite runs, which is what §12 claims
and what `RDirectBufferElem` groups 1–5 already assert.

**So: no claim in this record is contradicted by the screen, and the screen
cannot discharge §11 either.** §11's first bullet — "everything about CratonVM's
behaviour" — still stands in full: the §6 probe has never been run against a
`--features synthetic-jdk` + `--synthetic-jdk` binary, and that is the only
configuration in which any of §5's census is executed.
