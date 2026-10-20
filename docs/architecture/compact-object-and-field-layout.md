# Compact object and field layout

Status: implemented (8-byte compact header since 2026-09-24)

## Header contract

Every object starts with one 8-byte **header word**:

| Offset | Width | Meaning |
| ---: | ---: | --- |
| 0 | 4 | class id (a plain `u32`; JIT type guards are `CMP DWORD [recv+0], imm`) |
| 4 | 4 | mark word |

The mark word (`AtomicU32`, `MARK_WORD_OFFSET = 4`):

| Bits | Meaning |
| ---: | --- |
| 0..1 | state: `NEUTRAL`, `THIN_LOCKED`, `INFLATED`, `FORWARDED` |
| 2..15 | state payload (below) |
| 16..17 | `kind` (object / array / humongous filler) |
| 18..21 | `element_type` (arrays) |
| 22..23 | reserved, zero (arrays and legacy instances) |
| 24..27 | GC flags: `OLD_GEN`, `MARKED`, `COMPACT`, `HEADER` |
| 28..31 | GC age |

Bits 16..31 are the object's bytes 6 and 7: `KIND_TAGS_BYTE_OFFSET = 6`,
`GC_FLAGS_BYTE_OFFSET = 7`.

What follows the header word depends on the object:

* A **compact instance** (`GC_FLAG_COMPACT`) has nothing else: its fields start
  at `COMPACT_HEADER_SIZE` (8). Its field count is its class's registered
  compact layout's, so it needs no shape word.
* An **array** or a **legacy instance** (16-byte `Value` cells) has a second
  word: the shape (`ARRAY_LENGTH_OFFSET = NUM_SLOTS_OFFSET = 8`, array length or
  field count) and the identity hash (`IDENTITY_HASH_OFFSET = 12`). Its payload
  starts at `HEADER_SIZE` (16) = `ARRAY_DATA_OFFSET`.

Every object is at least `MIN_OBJECT_SIZE` (16) bytes, so every object has a
second word.

`ObjectHeader` is the 16-byte `#[repr(C)]` view of both words. The shape and
aux words are private: on a compact instance they are field storage, so every
read goes through an accessor that knows the header length
(`ObjectHeader::is_short`, `num_slots`, `array_length`, `payload_offset`).

## Mark-word states

* **NEUTRAL.** A compact instance keeps its identity hash here: 14 bits in the
  payload plus bits 18..23 (a plain object has no element type), 20 bits in all,
  reported with 11 class-derived bits on top (`ObjectHeader::short_hash_value`).
  A long header's hash is its aux word (31 bits) and does not touch the mark
  word.
* **THIN_LOCKED.** Payload = owner **lock slot** (11 bits) + recursion count
  (3 bits). A thread leases a slot from its VM's `LockSlots` the first time it
  thin-locks and gives it back when it dies, unless it still holds a thin lock.
  A thread with no slot, or a recursion past 7, inflates. A hashed compact
  instance cannot thin-lock (its hash occupies the word) and inflates, the hash
  moving into the monitor first.
* **INFLATED.** No payload: the `Monitor` is found by object address in the VM's
  sharded monitor index. Inflation publishes the mark word and the index entry
  under the index shard's lock, and every moving collector re-keys the index
  (`MonitorCleanup::remap_after_gc`; a moved entry wins over a retained stale
  one at the same address).
* **FORWARDED.** The header word keeps the class id and the quartet; the target
  is the object's **second word** (`FORWARDING_TARGET_OFFSET = 8`). A
  self-forward (`MARK_FWD_SELF`) writes nothing but the mark word. Parallel
  evacuators claim with one CAS to `FORWARDED | BUSY`, write the target, then
  clear `BUSY` (`try_claim_forwarding` / `publish_claimed_forwarding`); readers
  of a forwarded object's shape follow the target, and a shape read bracketed
  by two equal mark-word reads is the object's own. The sliding old-generation
  compactor resolves moves through a side map instead of the header, because it
  scans live bodies while the moves are pending.

**An interior address can look like a header.** With a one-word header,
`obj + 8` (a compact instance's first field) decodes as a class id and a mark,
and a heap pointer's upper half is a valid NEUTRAL plain-object mark. Any
path that takes an address it did not get from a reference field, such as a
conservative root from a JIT frame's saved-register image, must check
`GC_FLAG_HEADER` (stamped by every allocator) before it treats the address as
an object. G1's root-pin scan does, and pins such a root's region rather than
evacuating it. Before this check, evacuating an interior root wrote
`FORWARDED` into the upper half of the real object's forwarding target.

## Field representation

Compact instance fields are tagless and naturally aligned. Boolean and byte
use one byte, char and short use two, int and float use four, and references,
longs, and doubles use eight (four for references under compressed oops). The
object is rounded to an 8-byte boundary and to at least 16 bytes.

Each class layout (`CompactLayout`) contains ABSOLUTE field displacements
(`field_disps`, already including the 8-byte header), storage kinds, the
reference oop-map (`ref_disps`) used by every collector, and the object's
`total_size`. No consumer adds a header size to a compact displacement. All
compact reads and writes are width-correct atomic operations. Reference stores
use the GC barrier path when required.

## Layout stability

A compact instance's field count is read from its class's CURRENT layout, so a
class whose layout can change while instances exist must not be compact: a
compatibility stub, and every class inheriting from one, stays on the legacy
self-describing layout (`build_compact_layout` refuses it). Legal JVM
redefinition cannot change the field schema. Readers still key the layout
registry by `(class_id, field_count)`.

Layout metadata remains live at least as long as instances can be live.
Class-loader unloading is responsible for reclaiming versions only after the
loader and all instances are proven unreachable.

## Compatibility

The compact flag is per object. Legacy 16-byte `Value`-cell objects (with the
long header) remain readable in the same process, including objects of
compatibility-stub classes and padded synthetic containers. Arrays keep their
packed element representation, their long header and their data offset.

## Validation

Regression tests pin every header offset, the mark-word encodings (thin lock,
hash, forwarding, the claim/publish protocol), the compact-hash / thin-lock
interaction, round-trip every tagless storage kind, retain old layout versions,
and scan compact references in the supported collectors. The JIT contract tests
(`jit/src/x64/flag_and_header_contracts.rs`) pin the inline allocators' header
stores.
