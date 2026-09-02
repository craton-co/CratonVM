# Layout-constant hazards — making the existing constants safe to change

*Session slug: `layout-constant-hazards`. Written against
`arch/wave1-integration-20260726` merged at `9d0b477852d6516cf2e152127c38c1febc23d3db`.
Scope of edits: `types/src/lib.rs`, `jit/tests/intrinsic_arrays_ops.rs`,
`jit/src/ir_lower.rs`, `jit/src/lib.rs`, this file.*

This session changes **no layout value**. It is the preparation pass for the
`ObjectHeader` shrink described in `header-shrink.md`: it closes the one site
where a layout change fails *unsoundly*, un-strands three constants that landed
unreachable, and puts the second machine-code emitter into the inventory the
shrink navigates by.

---

## 1. The unsound fixture — `jit/tests/intrinsic_arrays_ops.rs` (closed)

`intrinsic_arrays_ops.rs:21-22` defined its own copies:

```rust
const HEADER_SIZE: usize = 32;
const ARRAY_LENGTH_OFFSET: usize = 12;
```

and laid `FakeArray` out with them. This is the only site in the workspace where
a layout change does **not** produce a failed assertion: the local copies keep
compiling, the fixture allocates and indexes at the old offsets, and the JIT
under test emits the new ones. `set_len` and the element reads then run past the
end of the backing `Vec` — memory corruption in a test process, and a test result
that means nothing either way.

**Now:** both constants come from `use cratonvm_types::{ARRAY_LENGTH_OFFSET,
HEADER_SIZE};`, exactly as every sibling test file already did. No frozen copy
was needed — the fixture has no reason to model a historical layout.

Two tripwires were added at the bottom of the file:

- `fake_array_layout_assumptions_hold_for_the_real_constants` — pins what
  `FakeArray` still assumes: `HEADER_SIZE == size_of::<ObjectHeader>()`,
  `HEADER_SIZE % 8 == 0`, `ARRAY_LENGTH_OFFSET + 4 <= HEADER_SIZE`, both offsets
  inside signed `disp8`, and that `FakeArray::new`'s +8 over-allocation still
  covers the object at whatever size the constants now describe.
- `no_test_file_restates_an_object_layout_constant` — walks every `.rs` file
  under a `tests/` directory in the workspace and fails on any `const`/`static`
  definition of a name `cratonvm_types` owns (16 names). It asserts it actually
  reached test sources, and specifically that it reached this file, so it cannot
  pass vacuously.

  Scoped to `tests/` trees deliberately: `HEADER_SIZE` is a legitimate and
  entirely unrelated name in `jfr/src/dump.rs` (JFR chunk header, 72),
  `reader/src/jimage.rs` (jimage file header, 28) and `vm/src/debug/protocol.rs`
  (debug wire header, 11). None describes the object header; none lays out heap
  fixtures.

**Other local redefinitions found in the sweep:** only one, and it is safe —
`jit/src/x64.rs:78` defines `IDENTITY_HASH_CODE_OFFSET` locally, but *derives* it
with `offset_of!(ObjectHeader, identity_hash_code)` rather than writing a
literal, so it cannot drift. Now that `types` exports the constant (§2), that
local derivation is redundant and should be replaced by the re-export the next
time `x64.rs` is opened. It is not this session's file.

---

## 2. The stranded constants — `types/src/lib.rs` (closed)

`mod heap_types` is **private** with an explicit re-export list. `MARK_FORWARDED`,
`FORWARDING_PTR_MASK` and `IDENTITY_HASH_CODE_OFFSET` all landed this wave as
`pub` inside that private module and were therefore unreachable from every other
crate — a default-off landing arrived at by omission. All three are now on the
list, with a comment above it explaining that the list is the only door.

`every_public_heap_constant_is_reachable` names all three from outside
`heap_types` (so dropping a re-export is a compile error) and checks the
encoding contract: the forwarding tag is distinct from the other three mark
states, `FORWARDING_PTR_MASK == !MARK_STATE_MASK`, and an aligned address
round-trips exactly through `addr | MARK_FORWARDED` → `mark & FORWARDING_PTR_MASK`.

### What replaced `assert_eq!(HEADER_SIZE, 32)`

`types/src/lib.rs:119` pinned a historical *value*, not an invariant. It records
what the header happened to be, so a deliberate and fully correct shrink trips
it while a careless change that keeps the size but breaks alignment does not —
and it cannot tell the reader of the failure *why* 32 mattered.

It was **replaced, not deleted**, with the properties that are load-bearing:

| Replaced check | With | Why it is the real constraint |
| --- | --- | --- |
| `HEADER_SIZE == 32` | `HEADER_SIZE % 8 == 0` | Heap walks, the inline-TLAB cursor bump and the qword body-zeroing loop all step from it; the 8-byte `mark_word` needs natural alignment for the lock fast-path CAS |
| | `HEADER_SIZE <= 127` | The JIT emits `[base + index*scale + HEADER_SIZE]` with a **signed** `disp8`. Past 127 the byte reads back negative and the load addresses memory *before* the object — no panic, just a wrong address |
| | `HEADER_SIZE >= MARK_WORD_OFFSET + 8` | No exported field offset may point outside the header |

Three additions in the same test, all previously unchecked at this level:
`ARRAY_LENGTH_OFFSET <= 127` (same `disp8` failure mode, for the bounds-check
load), `ARRAY_LENGTH_OFFSET + 4 <= HEADER_SIZE`, and
`IDENTITY_HASH_CODE_OFFSET + 4 <= HEADER_SIZE`. The pre-existing
`ARRAY_LENGTH_OFFSET > 0`, `SLOT_SIZE`, `REF_ELEMENT_SIZE`, `OBJECT_KIND_OFFSET`,
`ARRAY_ELEMENT_TYPE_OFFSET` and `AUTOBOX_CLASS_ID` checks are untouched.

`heap_types.rs` already pins `HEADER_SIZE <= 127` and `ARRAY_LENGTH_OFFSET <= 127`
at compile time. Restating them where the constants are *published* costs
nothing and means the failure message names the emitter that breaks.

---

## 3. The second emitter — `jit/src/ir_lower.rs` and `jit/src/lib.rs`

`ir_lower.rs` is a second x86-64 emitter alongside `x64.rs`, and the
header-offset inventory the shrink was planned from scanned `x64.rs` only.

**Complete site list** (line numbers after this session's edits; the
pre-session numbers from `header-shrink.md` §6.6 are in parentheses):

| Site | What it emits | Form |
| --- | --- | --- |
| `jit/src/ir_lower.rs:1186` (new 2026-07-31) | guarded inline compact `getfield`, `HEADER_SIZE + packed_body_offset` | disp32 |
| `jit/src/ir_lower.rs:1930` (1887) | field address, `HEADER_SIZE + field_index * SLOT_SIZE` | disp32 |
| `jit/src/ir_lower.rs:1966` (1923) | field **tag** address, same formula | disp32 |
| `jit/src/ir_lower.rs:2014` (1971) | `MOVSS/MOVSD XMM0,[RAX+RCX*n+HEADER_SIZE]` | **disp8** |
| `jit/src/ir_lower.rs:2035` (1992) | `MOVSS/MOVSD [RAX+RCX*n+HEADER_SIZE],XMM0` | **disp8** |
| `jit/src/ir_lower.rs:2647` (2604) | `MOV R10D,[RAX+ARRAY_LENGTH_OFFSET]` bounds check | **disp8** |
| `jit/src/lib.rs:3236` (3186) | `(HEADER_SIZE + body_off) as i32` — compact string field **payload** address | disp32 |
| `jit/src/lib.rs:3210` (3226) | `(HEADER_SIZE + idx * SLOT_SIZE) as i32` — legacy string field cell | disp32 |
| `jit/src/lib.rs` `AtomicIntFieldLayout::new` (new 2026-08-12) | `(HEADER_SIZE + body_off) as i32` — compact `AtomicInteger.value` **payload** address | disp32 |
| `jit/src/lib.rs` `AtomicIntFieldLayout::new` (new 2026-08-12) | `(HEADER_SIZE + idx * SLOT_SIZE) as i32 + FIELD_CELL_PAYLOAD32_OFFSET` — legacy `AtomicInteger.value` cell | disp32 |
| `jit/src/lib.rs` `AtomicLongFieldLayout::new` (new 2026-08-28) | `(HEADER_SIZE + body_off) as i32` — compact `AtomicLong.value` **payload** address | disp32 |
| `jit/src/lib.rs` `AtomicLongFieldLayout::new` (new 2026-08-28) | `(HEADER_SIZE + idx * SLOT_SIZE) as i32 + FIELD_CELL_PAYLOAD64_OFFSET` — legacy `AtomicLong.value` cell | disp32 |
| `jit/src/ir_lower.rs` `emit_inline_getstatic` (new 2026-08-03, cov-01) | direct `getstatic`: `field_index * SLOT_SIZE + FIELD_CELL_PAYLOAD{32,64}_OFFSET` from the class's statics-block base | disp32 |
| `jit/src/ir_lower.rs` `emit_gated_ir_ref_putfield` (new 2026-09-02) | gated compact reference **store**, `HEADER_SIZE + packed_body_offset` — the mirror of the compact `getfield` read, `MOV [RAX+disp32], RDX` | disp32 |
| `jit/src/ir_lower.rs` `emit_gated_ir_ref_putfield`, LEGACY shape (new 2026-09-02) | the same store into the uniform 16-byte cell, `HEADER_SIZE + field_index * SLOT_SIZE` plus the tag and `FIELD_CELL_PAYLOAD64_OFFSET` biases; picked per OBJECT on `GC_FLAG_COMPACT`, so a shrink must move it and the compact shape together | disp32 |
| `jit/src/ir_lower.rs` `emit_inline_compact_getfield`, LEGACY branch (new 2026-08-18) | `HEADER_SIZE + field_index * SLOT_SIZE + FIELD_CELL_PAYLOAD{32,64}_OFFSET` — the uniform 16-byte `Value` cell, four emitted forms (ref / `J`\|`D` qword, `F` zero-extending dword, int-category `MOVSXD`) | disp32 |

**2026-08-03, COV-02** (`docs/internal/cov-02-array-element-access-RETIRED-20260803.md`)
added three more `ir_lower.rs` sites, all **disp8**, all checked by
`disp::disp8_const` rather than narrowed with a raw cast:

| Site | What it emits | Form |
| --- | --- | --- |
| `emit_gpr_array_elem_load` | `[RAX+RCX*{1,2,4,8}+HEADER_SIZE]` for `iaload`/`laload`/`baload`/`caload`/`saload`/`aaload` | **disp8**, checked |
| `emit_gpr_array_elem_store` | the same address for `iastore`/`lastore`/`bastore`/`castore`/`sastore` | **disp8**, checked |
| `Op::ArrayLength`'s lowering arm | `MOV EAX,[RAX+ARRAY_LENGTH_OFFSET]` for `arraylength` | **disp8**, checked |

Each emitter materialises the header displacement **once** and shares it across
every element width, which is why eleven new instruction encodings cost two new
sites rather than eleven. Preserve that when the shrink lands: a per-width copy
of the constant would be eleven places to revisit, and this file would be the
only thing that noticed.

**All eight originals already read the shared constants, not literals** — no site needed
converting. The hazards are the ones the inventory was supposed to surface and
did not:

1. The three `disp8` sites narrow the constant with an *unchecked* cast into a
   literal instruction byte array. Above 127 the displacement is negative and
   the emitted load addresses backwards from the object base. Restated as
   `const _: () = assert!(…)` at the top of `ir_lower.rs`, together with the
   8-byte grid property that the `HEADER_SIZE + index*SLOT_SIZE` formula needs,
   so the constraint travels with the code that depends on it.
2. Neither file was inventoried.

### Verifying the tripwire mechanism before extending it

`x64.rs::header_offset_emission_site_inventory_matches_the_doc` counts the
substring `<CONST> as <ty>`. Re-run against the merged tree, its recorded totals
are exact: `HEADER_SIZE as u8` 31, `HEADER_SIZE as i32` 11,
`ARRAY_LENGTH_OFFSET as u8` 16, `ARRAY_LENGTH_OFFSET as i32` 5. The sibling
`ir_lower_header_offset_sites_are_inventoried_too` is likewise exact at 2 / 2 / 1,
and this session's edits deliberately avoid that needle's spelling so both
totals still hold. (COV-02 gave that sibling a fifth row and moved two of its
counts: `HEADER_SIZE as u8` stays 2 — the FP arms — a new `HEADER_SIZE as i64`
row is 2 for the two checked GPR emitters, and `ARRAY_LENGTH_OFFSET as i64`
went 1 → 2 for `arraylength`'s own length load.)

**But the mechanism is structurally blind to a case that matters.** The needle
requires the constant to be *immediately* followed by a cast. Both `lib.rs` sites
are written as

```rust
let abs = (cratonvm_types::HEADER_SIZE + body_off) as i32;
(cratonvm_types::HEADER_SIZE + idx * cratonvm_types::SLOT_SIZE) as i32
```

for which a substring scan reports **zero** hits. That is the mechanical reason
`lib.rs` never appeared in the header-offset inventory even after `x64.rs` and
`ir_lower.rs` had been audited: the tool could not see it, so no amount of care
from the auditor would have. Extending the substring tripwire to `lib.rs` would
have produced a passing test that checks nothing — the exact failure mode this
repo has hit before.

### What was added instead

`jit/src/lib.rs::layout_constant_inventory` counts **identifier occurrences in
code** — every use of the constant in any expression shape, excluding comments
and string literals — for both `lib.rs` and `ir_lower.rs`, across all eight
layout constants this crate could plausibly emit:

| | `HEADER_SIZE` | `ARRAY_LENGTH_OFFSET` | `SLOT_SIZE` | `REF_ELEMENT_SIZE` | `MARK_WORD_OFFSET` | `IDENTITY_HASH_CODE_OFFSET` | `FIELD_CELL_PAYLOAD32_OFFSET` | `FIELD_CELL_PAYLOAD64_OFFSET` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `jit/src/lib.rs` | 7 | 1 | 4 | 1 | 0 | 0 | 2 | 2 |
| `jit/src/ir_lower.rs` | 17 | 4 | 7 | 0 | 0 | 0 | 6 | 6 |

(The `ir_lower.rs` row read `7 | 3 | …` when this section was written, went to
`8` with the 2026-07-31 guarded inline compact `getfield`, to `10 | 4` with
COV-02's two GPR array emitters and `arraylength`'s length load, to
`… | 5 | … | 4 | 2` with cov-01's `emit_inline_getstatic`, and to
`11 | 4 | 6 | … | 6 | 4` with the 2026-08-18 IR inline `getfield` LEGACY arm.
The `lib.rs` row went `3 → 5` with `AtomicIntFieldLayout` on 2026-08-12 and
`5 → 7` with `AtomicLongFieldLayout` on 2026-08-28. The authority is
`INVENTORY` in `jit/src/lib.rs`, which is executed; this table is a copy, it
had already drifted by one before COV-02 re-derived it, and it had drifted
again — by four rows' worth — before 2026-08-28 re-derived it from the
executed table. **Read `INVENTORY`, not this.**)

The zero entries are as load-bearing as the rest: a constant that starts being
used in a file where it never appeared before also trips the assertion and forces
the new site into the inventory.

**2026-08-03 update — cov-01.** Its contribution to the row above is the fifth
`SLOT_SIZE`, the fourth `FIELD_CELL_PAYLOAD32_OFFSET` and **both**
`FIELD_CELL_PAYLOAD64_OFFSET`s — the `use` list and one site,
`emit_inline_getstatic`. (The `HEADER_SIZE` 8 → 10 and `ARRAY_LENGTH_OFFSET`
3 → 4 in the same row are COV-02's, landed the same day; the two lanes touched
disjoint sites and the counts simply add.)

That site is value-safe at any header size for a reason worth stating rather
than assuming: it addresses a **statics block**, which has no object header at
all, so no `HEADER_SIZE` term appears in it. What it does bake is the
`field_index * SLOT_SIZE + payload_offset` cell arithmetic, which is the same
16-byte `Value` cell shape `types::heap_types::field_cell_layout_matches_value_enum`
pins for instance fields. Both encodings are disp32
(`48 8B 80 disp32` for a reference, `48 63 80 disp32` for an int-category
value), so it does not share the disp8 backwards-addressing hazard the three
array/element sites have.

**2026-07-26 update — the inventory earned its keep, in reverse.** The
`jit/src/lib.rs` row read `… 0 | 2` because `StringFieldLayout::new` had a
single `cell()` closure that biased *both* the compact and the legacy branch by
`FIELD_CELL_PAYLOAD64_OFFSET` and never mentioned `FIELD_CELL_PAYLOAD32_OFFSET`
— which is exactly the shape of the bug it was hiding. Every `x64.rs` call site
added its own payload offset on top, so for a COMPACT instance (whose
`CompactLayout` offset is already the payload address, no tag word) `coder` and
`hash` were each read 4 bytes high: `coder` landed on `hash`, `hash` landed on
`hashIsZero`. `String.length()` therefore computed
`value.length >> (hash & 31)` for any receiver whose lazy hash cache had been
populated. See `docs/internal/fixed-suite-bugs/h2-suite-bugs/h2-jitban-schema-not-found-on-reconnect-FIXED.md`.
The row is now `… 1 | 1`: `legacy()` uses each payload constant exactly once
(ref vs int-category) and `compact()` uses neither.

Two self-checks guard the guard:

- `the_counter_counts_what_its_name_says` runs the counter over a sample whose
  answer is obvious by inspection, proving it excludes whole-line, doc and
  trailing comments and string literals, never matches inside a longer
  identifier, and *does* count the parenthesised-then-cast form — and asserts on
  the same sample that the substring needle finds none of it.
- `the_inventory_is_reading_real_source` fails if either `include_str!` comes
  back implausibly short or if the whole table is zeros, so the inventory cannot
  pass vacuously.

The counter's two known limitations only ever *under*-count, and both are pinned
by the fixed totals: a `"` inside a character literal makes the rest of that line
read as a string, and a string literal continued across a line break is treated
as re-opening on the next line.

---

## 4. Handoff

Nothing here blocks the shrink; it removes obstacles from it.

- The shrink owner can now change `HEADER_SIZE` without silently corrupting
  `intrinsic_arrays_ops.rs`, and without hunting for a value assertion that only
  records history.
- `header-shrink.md` §6.5 (the unsound fixture) and §6.2 (the stranded
  constants) are **closed**. §6.6 is closed for inventory coverage; its
  conclusion that the sites are value-safe at `HEADER_SIZE = 24` is unchanged
  and was not re-litigated.
- When `header-shrink.md` §6.1 lands, the `ir_lower.rs` / `lib.rs` counts in §3
  above will not move (no site is added or removed by a pure value change), but
  the `types/src/lib.rs` invariants will start describing a 24-byte header
  without any edit — which is the point.
- Follow-up for whoever next owns `jit/src/x64.rs`: replace its locally derived
  `IDENTITY_HASH_CODE_OFFSET` (`x64.rs:78`) with the now-exported constant, and
  fix the stale "Layout reminder" block in `vm/src/jit/helpers.rs:2247-2257`
  that still describes the 40-byte header (`header-shrink.md` §6.9).
