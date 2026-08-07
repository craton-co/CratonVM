# Header 24 → 16, and where the object bloat actually is

*Continues `header-shrink.md`, which planned and landed 32 → 24. That document's
§3 ("Why 16 is blocked") and §5 ("Per-object savings") are both **superseded**
here: §3 named the wrong blocker, and §5's arithmetic for compact bodies was
already stale when it was written.*

---

## 1. §5's arithmetic was stale: primitive fields are already packed

`header-shrink.md` §5 states `body_compact = 8*refs + 16*prims`, and §5.1 ranks
"primitive field packing (`SLOT_SIZE` 16 → natural width)" as the **largest**
remaining win, ahead of both header steps.

That is not what the code does. `FieldStorageKind::size()` returns 1/2/4/8 and
`build_compact_layout` (`classloading/src/class.rs`) has laid every compact
field out at its natural Java width, at natural alignment, since
`d46e70521 gc: compact object headers and field storage`. `SLOT_SIZE = 16` is
only the **legacy** cell — the fallback for classes that are refused a compact
layout.

Measured on a real run (`CRATONVM_DBG_LAYOUT=1`, 295 classes with a compact
layout): `java/lang/String` had `body=24`, not the 56 that `8*1 + 16*3` predicts.
So the ranking in §5.1 is wrong, and the item at the top of it was already done.

### 1.1 What *was* still on the table: the assignment order

Fields were stored at their natural width but assigned offsets in **declaration
order**, which spends real bytes on alignment gaps. `String` is the case:

| field | width | declaration order | widest-first |
| --- | ---: | ---: | ---: |
| `byte[] value` | 8 | 0 | 0 |
| `byte coder` | 1 | 8 | 12 |
| `int hash` | 4 | 12 | 8 |
| `boolean hashIsZero` | 1 | 16 | 13 |
| | | **body 24** | **body 16** |

14 bytes of field data; declaration order rounds to 24, widest-first to 16.

**Landed** as `CRATONVM_PACK_FIELDS_BY_WIDTH` (default on, `=0` restores
declaration order). Census over the same 295 classes: 20 shrank, total compact
body bytes 6312 → 6128.

### 1.2 Width-descending alone makes some objects BIGGER

`java.util.LinkedList` grew **24 → 32** in the first census, which is why the
measurement mattered and reading the sort did not.

`AbstractList` contributes `int modCount` and leaves the running offset at 4.
Declaration order then happens to fill that 4-byte hole with `int size` before
the two `Node` references and lands on 24. Widest-first put an 8-byte reference
there instead, wasted the hole, ended at 28, and rounded to 32.

The fix is **hole-first** placement: at each step take the widest remaining
field that needs no padding where the cursor already is, and pay for padding
only when nothing fits. Plus a per-ancestor fallback to declaration order if the
packed result is still larger — which makes "never larger than declaration
order" a property of the output rather than a hope about the heuristic.

The reorder is scoped to **one ancestor's own contribution**, not the flattened
field list. That is load-bearing: it is what keeps a parent's fields on
identical offsets whether the layout being built is the parent's own or the
prefix of a child's, which is the property that lets a `getfield` through a
supertype-typed reference use a single offset table.

---

## 2. Why 16 is blocked — the real reason

§3 said 16 requires deleting the `kind`/`element_type`/`gc_age`/`gc_flags` word
and that this needs a HotSpot-style displaced header inside `Monitor`. Both
halves are wrong.

The actual constraint is one line of arithmetic:

> `mark_word` is an `AtomicU64`, so it is 8-aligned. A 16-byte header therefore
> has **at most 8 bytes** before it. Everything that is not the mark word must
> fit in 64 bits.

What has to fit in those 64 bits:

| | bits | note |
| --- | ---: | --- |
| `class_id` | 32 | must stay a plain `u32` load at offset 0 — JIT contract |
| `shape` (array length) | 31 | `Integer.MAX_VALUE`; irreducible per object |
| `kind` | 2 | 3 values |
| `element_type` | 4 | tags 0, 4..11 |
| `gc_age` | 4 | 0..15 |
| `gc_flags` | 3 | OLD_GEN, MARKED, COMPACT |
| **total** | **76** | |

76 > 64. **A fixed 16-byte header with a full 31-bit array length is
arithmetically impossible**, whatever is done with the identity hash or with
`Monitor`. Deleting `identity_hash_code` (32 bits) is necessary but nowhere near
sufficient, and on its own buys **zero** bytes — the 4 freed bytes reappear as
alignment padding, exactly as §1 of the predecessor already established.

### 2.1 What does fit

Objects do not need `element_type`, and an object's `shape` is a field count,
not an array length — the JVMS caps `fields_count` at `u16` per class, and this
VM already screens it at `1 << 24` (`MAX_PLAUSIBLE_LEGACY_SLOTS`). So for
objects:

```
offset 0..4   class_id : u32          plain 32-bit load — JIT contract intact
offset 4..8   packed   : AtomicU32
                bits 0..2    kind
                bits 2..6    element_type
                bits 6..10   gc_age
                bits 10..13  gc_flags
                bits 13..32  object field count (19 bits = 524_287)
offset 8..16  mark_word : AtomicU64
```

= **16 bytes for a non-array object.**

Arrays keep their length in an 8-byte prefix at the head of the body (length at
offset 16, padding to 24, data at 24) — so an array's total size is
**unchanged**, and array element addressing lands on the same displacement it
has today.

| | today | target |
| --- | ---: | ---: |
| non-array object | 24 + body | **16 + body** |
| array | 24 + data | 24 + data |
| `String` | 24 + 24 = 48 | 16 + 16 = **32** (HotSpot 24) |
| 2-ref tree node | 24 + 16 = 40 | 16 + 16 = **32** (HotSpot 24) |

### 2.2 The identity hash has somewhere to go, and the lock path needs no change

`identity_hash_code` moves into the mark word's `NEUTRAL` upper bits, lazily
installed, exactly as HotSpot does it. The reason this is cheap here is a detail
of the existing code:

```rust
pub fn try_thin_lock(header: &ObjectHeader, thread_id: u32) -> Result<(), u64> {
    header.mark_word.compare_exchange(types::MARK_NEUTRAL, ...)
```

The fast path CASes against the **literal** `MARK_NEUTRAL` (`== 0`), not against
`mark_state(cur) == NEUTRAL`. A word carrying a hash is non-zero, so the CAS
fails on its own and the caller falls through to `inflate_locked` — which is
precisely HotSpot's "a hashed object cannot be thin-locked, it inflates" rule,
already implemented, for free. **No change to the locking fast path.**

That leaves exactly one transition that can destroy a hash: `publish_inflated`.
It is cold, it is one function, and it already has the pre-CAS word (`cur`) in
hand, so it is the single place that has to displace the hash. There is no
deflation in this VM (`grep deflat` in `vm/src/threading/monitor.rs` is empty),
so once displaced it never has to come back.

The displaced hash cannot live in `Monitor`: `gen_heap.rs::identity_hash_code`
is in the `gc` crate and `MonitorTable` is in `vm`, so a `gc → vm` edge would be
required. It goes in an address-keyed side table in `gc`, remapped by the
collector the way `MonitorTable::remap_after_gc` already is.

### 2.3 The real cost, counted

Not the design — the edit surface. Counted on the tree, production `src/` only:

| | sites |
| --- | ---: |
| `HEADER_SIZE` | 861 |
| — of which the array-element idiom (`HEADER_SIZE + idx * width`) | 79 |
| — of which JIT displacement bakes (`HEADER_SIZE as i32/u8`) | 55 |
| `ARRAY_LENGTH_OFFSET` / `NUM_SLOTS_OFFSET` | 119 |
| `.gc_flags` / `.gc_age` / `.kind` / `.element_type` / `.shape` | ~690 |
| `.identity_hash_code` (true field reads, not `ctx.identity_hash_code(o)`) | 34 |

The 533-occurrence figure that made the predecessor call the identity hash "the
field with the widest reader surface" counted `ctx.identity_hash_code(obj)`
**method calls**. The true field surface is 34 sites in 16 files, 24 of them in
`gc/` and `types/`. It is the *smallest* of these, not the largest.

The dangerous number is the first one. `HEADER_SIZE` means "start of the object
body" at some sites and "start of the array data" at others, and today those are
the same integer — so **the source does not record which is which**, and the
target needs them to differ (16 vs 24). Splitting them is the actual work, and a
single misclassification is silent heap corruption surfacing as a rare SIGSEGV
in a suite run, not a compile error.

### 2.4 How to make that split safe

1. Introduce `ARRAY_DATA_OFFSET`, defined equal to `HEADER_SIZE`, and migrate
   every array-data site to it as a **no-op refactor**. Nothing changes
   behaviourally; what changes is that the classification is now recorded in the
   source and reviewable on its own.
2. Same for the quartet: convert raw `.gc_flags` / `.kind` / … accesses to
   accessor methods first, so the later bit-packing is a change in one place.
   (`convert the IDIOM, not the sites`.)
3. **Positive control before trusting step 1.** Build once with
   `ARRAY_DATA_OFFSET = 64` — deliberately, absurdly wrong — and run the suite.
   Every site that should have been migrated and was not now reads 40 bytes off
   its own array and crashes immediately. A green suite under a *correct*
   `ARRAY_DATA_OFFSET == HEADER_SIZE` proves nothing at all, because the two
   constants are equal: the refactor is untestable by construction until it is
   made unequal on purpose.

Step 3 is the one that makes the difference between this landing and this
landing *quietly broken*. `ARRAY_DATA_OFFSET == HEADER_SIZE` is the definition
of a guard that cannot fail.

---

## 3. 12 bytes is not reachable

`HEADER_SIZE % 8 == 0` is a hard `const` assert whose message states the reason:
the TLAB bump grid and the qword-indexed body cells desync otherwise — silently,
in release. HotSpot reaches 12 (8 mark + 4 compressed klass) because its object
fields pack at 4-byte alignment and array data starts at 16. CratonVM's body is
qword-indexed throughout, so 12 requires redoing the **body** model first, not
the header.
