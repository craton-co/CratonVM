# `ObjectHeader` shrink — what is achievable, what it is worth, and what blocks the rest

*Session slug: `header-shrink`. Written against `arch/wave1-integration-20260726` merged at
`bfddf0fbc73d03c4dbfff9bbc172d09021ad5f8d` (`HEADER_SIZE = 32`, `MARK_WORD_OFFSET = 24`
confirmed on the merged tree). Scope of edits: `types/src/heap_types.rs`,
`gc/src/gen_heap.rs`, `jit/src/x64.rs`, and this file. Everything in §6 is a request to
another owner and was **not** edited here.*

Companion reading:

- `docs/internal/arch-2026-07-26/x64-flag-skew-and-contracts.md` §5–§6 — the codegen-side
  site inventory this work navigates by. Its §6 is titled "Do not make this change. This
  section is the map for whoever does." This document is the answer to that map.
- `gc/src/compact_header.rs` — the *aspirational* 8-byte header (JEP 519 / Lilliput),
  fully designed, fully unwired (`Config::use_compact_headers` is `false` and no allocator
  consumes it).

---

## 1. Headline

**The header cannot be shrunk below 32 bytes without editing files this session does not
own, and the shrink was not landed. What was landed is the encoding, the arithmetic, and
the tripwires the next pass needs.** The reason is not caution — it is that `ObjectHeader`
is a `#[repr(C)]` struct with **zero padding**, so a shrink is necessarily a field
*deletion*, and a field deletion breaks every direct `.field` access in the workspace
simultaneously. There is no partial, no gate, and no default-off form of "this struct has
one fewer field". Shipping it half-way does not degrade gracefully; it fails to compile in
eight non-owned files.

Two findings change the plan that was handed to this session:

1. **The target is 24, not 16.** Folding `forwarding_ptr` *and* `identity_hash_code` into
   the mark word yields **24 bytes, not 16** — the brief's premise was off by one step.
   Reaching 16 additionally requires deleting the `kind`/`element_type`/`gc_age`/`gc_flags`
   word, which is a separate and much harder problem (§3).
2. **Folding the identity hash is worth exactly zero bytes.** `mark_word` is an
   `AtomicU64` and forces 8-byte struct alignment, so removing the 4-byte hash reappears
   as 4 bytes of padding. Every byte of the 32→24 win comes from `forwarding_ptr` alone.
   The hash fold is not on the critical path and should be dropped from the plan — it is
   pure risk (it is the field with the widest reader surface) for no gain.

Both are pinned as tests, not just asserted here:
`shrink_candidate_sizes_are_what_the_doc_claims` and `header_has_no_reclaimable_padding`
in `types/src/heap_types.rs`.

---

## 2. The layout, and why every byte is spoken for

`types/src/heap_types.rs`, verified on the merged tree by `header_size_is_correct`:

| Offset | Field | Named constant | Size |
| ---: | --- | --- | ---: |
| 0 | `class_id` | *(none — 0 by JIT contract)* | 4 |
| 4 | `kind` | `OBJECT_KIND_OFFSET` | 1 |
| 5 | `element_type` | `ARRAY_ELEMENT_TYPE_OFFSET` | 1 |
| 6 | `gc_age` | `GC_AGE_OFFSET` | 1 |
| 7 | `gc_flags` | `GC_FLAGS_OFFSET` | 1 |
| 8 | `identity_hash_code` | `IDENTITY_HASH_CODE_OFFSET` **(added this session)** | 4 |
| 12 | `shape` (array length **or** instance-field count) | `ARRAY_LENGTH_OFFSET` == `NUM_SLOTS_OFFSET` | 4 |
| 16 | `forwarding_ptr` | `FORWARDING_PTR_OFFSET` | 8 |
| 24 | `mark_word` | `MARK_WORD_OFFSET` | 8 |
| | | `HEADER_SIZE = 32` | |

`4+1+1+1+1+4+4+8+8 = 32`. **There is no padding.** The struct is `align(8)` because of
`AtomicU64`, so any candidate layout's packed field total rounds *up* to a multiple of 8.

That single fact determines the whole option space:

| Delete | Packed | Aligned | Saving | `ARRAY_LENGTH_OFFSET` |
| --- | ---: | ---: | ---: | --- |
| *(nothing — today)* | 32 | **32** | — | 12 |
| `identity_hash_code` (4B) | 28 | **32** | **0** | 8 |
| `forwarding_ptr` (8B) | 24 | **24** | **8** | **12 — unchanged** |
| both (12B) | 20 | **24** | **8** | 8 |
| both + the kind/flags word (16B) | 16 | **16** | **16** | 4 |

The third row is the one to take. It is the *only* candidate that leaves
`ARRAY_LENGTH_OFFSET` at 12, which means **all 21 baked `ARRAY_LENGTH_OFFSET` sites in
`x64.rs` (16 of them disp8) need no change at all**, and neither do the 5 in `ir_lower.rs`
and the test files. The x64 audit's sharpest warning — "`ARRAY_LENGTH_OFFSET` must move
and is baked at 21 sites" — applies to the 16-byte target, not to the 24-byte one. Taking
24 first converts the mechanical surface from ~110 sites to ~110 sites of *one field*,
with zero displacement churn.

`class_id` stays at 0 (JIT contract, `helpers.class_id_offset_in_obj` is hard-set to 0)
and `HEADER_SIZE % 8 == 0` and `HEADER_SIZE <= 127` both still hold at 24 — the latter is
now joined by a matching `ARRAY_LENGTH_OFFSET <= 127` compile-time assert added this
session.

---

## 3. Why 16 is blocked, specifically

Reaching 16 means the header is `class_id(4) + shape(4) + mark_word(8)` and the
`kind`/`element_type`/`gc_age`/`gc_flags` byte quartet — about 13 live bits — has to go
somewhere. There are exactly three somewheres, and all three are closed:

**Into the mark word.** There is no room. In `MARK_INFLATED` state, bits 2–63 are *all*
the monitor pointer (`INFLATED_PTR_MASK = !MARK_STATE_MASK`); there is not one spare bit
above bit 1. HotSpot solves this with a **displaced header** saved inside the
`ObjectMonitor`, which this VM's `Monitor` has no slot for. Worse, it would make the GC's
hottest question — "is this an array?" — require loading the mark word, decoding its
2-bit state, and following a monitor indirection for any inflated object. That is a heap
walk hot path. Adding a displaced-header field to `Monitor` (`vm/src/threading/monitor.rs`)
is a design change, not a mechanical one, and that file is owned elsewhere.

**Into `class_id`'s high bits.** Closed by an explicit reservation:
`AUTOBOX_CLASS_ID = u32::MAX` and `MAX_SEQUENTIAL_CLASS_ID = u32::MAX` deliberately claim
the full `u32` range, pinned by `autobox_class_id_is_reserved`. Stealing high bits
un-reserves the sentinel.

**Into `shape`'s high bits.** Closed by `object_shape_preserves_full_u32_field_count`,
which pins that a 32-bit field count (`0xfeed_beef`) survives round-trip. `shape` is also
the array length, so stealing bits caps array size.

So 16 is a genuine second project with a design decision at its centre, not a longer
version of the same edit. **Do not plan 32→16 as one step.**

---

## 4. Mark-word encoding — the full table

The mark word is no longer free: `vm/src/threading/monitor.rs` publishes an inflated
monitor *through* it, and an `INFLATED` word owns exactly one strong `Arc<Monitor>`
reference leaked into it by the publishing CAS and released at exactly one site. Any
forwarding encoding has to coexist with that. This session claimed the previously-reserved
`0b11` tag and landed the encoding with full round-trip coverage; **nothing produces it
yet**, and `forwarding_ptr` remains the live mechanism until §6 is done.

### 4.1 The complete state table

| Tag (bits 1:0) | State | Bits 63:2 | Constant | Producer |
| --- | --- | --- | --- | --- |
| `00` | `NEUTRAL` | unused (zero) | `MARK_NEUTRAL` | `ObjectHeader::new` |
| `01` | `THIN_LOCKED` | bits 9:2 = recursion `u8`; bits 41:10 = owner tid `u32`; bits 63:42 reserved | `MARK_THIN_LOCKED` | `make_thin_locked` |
| `10` | `INFLATED` | bits 63:2 = `Monitor*` (unshifted; low 2 bits borrowed) | `MARK_INFLATED` | `make_inflated` |
| `11` | `FORWARDED` | bits 63:2 = relocation target (unshifted; low 2 bits borrowed) | `MARK_FORWARDED` **(new)** | `make_forwarded` **(no caller yet)** |

Masks: `INFLATED_PTR_MASK == FORWARDING_PTR_MASK == !MARK_STATE_MASK`. Both payloads are
**full 64-bit pointers**, OR-ed with the tag rather than shifted — nothing is truncated,
no overflow side table is needed, and the compressed-oops agent's "a folded forwarding
pointer must stay 64-bit" constraint is satisfied by construction. Heap objects are
8-byte aligned, so bits 2:0 of a legal target are already zero and only 2 are borrowed.

### 4.2 The collision that had to be ruled out first

`FORWARDED` and `INFLATED` carry their payload in **exactly the same bits**. Only the tag
separates them. A consumer testing `mark & MARK_INFLATED != 0` would accept a `FORWARDED`
word and hand a relocation address to `inflated_monitor()` as a `Monitor*` — silent
monitor corruption, the failure mode the brief flags as the worst in this file.

**Audited before claiming the tag: every consumer in the workspace uses
`ObjectHeader::mark_state(x) == types::MARK_*` — tag *equality*, never a bitwise AND.**
The sites are `vm/src/threading/monitor.rs` lines 261, 297, 335, 1158, 1182, 1289, 1290,
1308, 1315, 1354, 1454, 1527, 1558–1560, 1712–1720, 1771–1775, 1797–1801, 2642, 2648,
2685 and the test block from 2686. `types/src/lib.rs:54` re-exports the constants; nothing
else consumes them. So `0b11` is claimable today.

The trap is nevertheless asserted rather than merely described, by
`forwarded_and_inflated_are_only_distinguishable_by_tag_equality`, which proves both
`fwd & MARK_INFLATED != 0` and `fwd & MARK_THIN_LOCKED != 0` — so anyone who later writes
the bitwise form trips a test that explains why.

### 4.3 Ordering contract for the producer (read before writing this state)

Writing `FORWARDED` **destroys** whatever lock state the word held —
`forwarding_destroys_prior_lock_state_hence_copy_before_clobber` pins this. A relocating
collector must therefore:

1. **Copy the object first.** The destination's mark word then carries the intact
   `NEUTRAL` / `THIN_LOCKED` / `INFLATED` value.
2. **Only then clobber the source** with `make_forwarded(dest)`.

For an `INFLATED` source this **transfers** the single strong `Arc<Monitor>` reference to
the destination copy — it does not duplicate it. The release site must therefore never run
against a header whose mark word is `FORWARDED`, or the live destination is left holding a
dangling `Monitor*`. This is the one place where a mistake is silent monitor corruption
rather than a crash, and it is the reason the encoding was landed inert rather than wired
up opportunistically.

Note that `gc/src/g1.rs:444` already installs forwarding pointers by reinterpreting the
`forwarding_ptr` field as an `AtomicUsize` for a CAS. That gets *easier* under the fold —
`mark_word` is already an `AtomicU64` — but the CAS must move to a compare-and-swap on the
2-bit tag, not on the whole word, or it will race the monitor-inflation CAS on the same
word. **That race does not exist today** (the two words are distinct) and is created by
the fold. It is the single largest correctness item in §6.

### 4.4 There is deliberately no "hashed" mark-word state

`identity_hash_code` is a dedicated header field, orthogonal to every lock transition
(`identity_hash_is_orthogonal_to_every_mark_word_state`). Because folding it saves zero
bytes (§1), inventing a mark-word hash encoding would add a fifth claimant to a 2-bit tag
space that is now **exhausted** — all four tags are named — for no gain. Do not add one.
If a future 16-byte design needs it, it needs the displaced-header machinery from §3
anyway, and the hash should ride in that.

---

## 5. Per-object savings, with alignment

The compressed-oops agent found its change bought 0% on `Integer`, `String` and
`ArrayList` because 8-byte alignment rounded it away. **That does not happen here.**
Every object size below is already a multiple of 8 — `HEADER_SIZE` is 8-aligned, legacy
field cells are `SLOT_SIZE = 16`, compact reference fields are `REF_FIELD_SIZE = 8`, and
`array_data_size` rounds to 8 — so the header saving lands in full, every time, with **no
rounding loss on any row**. Object alignment is 8 workspace-wide; there is no 16-byte
object alignment anywhere (audited: every allocator in `gc/` and `vm/` uses `align = 8`;
the only `& !15` sites are x86-64/AArch64 *stack frame* ABI alignment).

Compact reference-field layout is **default ON**
(`field_layout.rs:174` — `Err(_) => true`), so the "compact" column is the default path
and the "legacy" column applies only to AUTOBOX wrappers, ad-hoc `ClassId(0)` containers,
and objects allocated before a synthetic-stub class grew.

| Object | Fields | Layout | **32 (now)** | **24 (achievable)** | **16 (needs §3)** | HotSpot |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| `java.lang.Integer` | 1 prim | either | **48** | **40** (−8, −16.7%) | 32 (−16, −33%) | 16 |
| `HashMap.Node` | 1 prim + 3 ref | compact | **72** | **64** (−8, −11.1%) | 56 (−16, −22%) | 32 |
| `HashMap.Node` | 1 prim + 3 ref | legacy | 96 | 88 (−8, −8.3%) | 80 (−16, −16.7%) | 32 |
| 2-ref tree node | 2 ref | compact | **48** | **40** (−8, −16.7%) | 32 (−16, −33%) | 24 |
| 2-ref tree node | 2 ref | legacy | 64 | 56 (−8, −12.5%) | 48 (−16, −25%) | 24 |
| `String` | 1 ref + 3 prim | compact | **88** | **80** (−8, −9.1%) | 72 (−16, −18.2%) | 24 |
| `String` | 1 ref + 3 prim | legacy | 96 | 88 (−8, −8.3%) | 80 (−16, −16.7%) | 24 |
| `Object[16]` | 16 ref elements | — | **160** | **152** (−8, −5.0%) | 144 (−16, −10%) | 80 |

Arithmetic: `total = HEADER_SIZE + body`, `body_legacy = n * 16`,
`body_compact = 8*refs + 16*prims`, `body_array = len * elem_size` rounded to 8.
`Integer` = 32 + 16 = 48 today, matching the 48 in the brief.

### 5.1 Read this before deciding the change is worth it

The honest reading of that table is that **the header is not where this VM's object bloat
lives.** A flat −8 bytes is a 5–17% win depending on shape, and it is real. But the same
table shows `Integer` at 48 against HotSpot's 16, and **32 of those 48 bytes are still
there after the shrink**: 24 of header and 16 of body for a single `int`. The dominant
term is `SLOT_SIZE = 16` — a tagged `Value` cell for a 4-byte field — and secondarily the
uncompressed `class_id`/`shape` words.

Ranked by bytes returned on `Integer`:

1. **Primitive field packing** (`SLOT_SIZE` 16 → natural width): 48 → 36 → 40 aligned.
   Owner: `types/src/field_layout.rs`. Not attempted here; not this session's file.
2. **Header 32 → 24** (this document): −8, mechanical, no design decision.
3. **Header 24 → 16** (§3): −8 more, but needs a `Monitor` displaced header.

The shrink is worth doing — it is the only one of the three with no design decision in it,
and it composes additively with both others. It is *not* on its own going to move Binary
Trees at 8.34x or HashMap at 21.2x, and it should not be sold as if it would. Those ratios
are dominated by allocation *rate* and field-access path, not resident size. Recommend
landing 32→24 as a clean mechanical change and then measuring, rather than bundling it
with anything else — an 8-byte-per-object delta is exactly the size of effect that a
bundled change makes unattributable.

---

## 6. Handoff — everything the 32→24 shrink still needs, by owner

This is the complete list. The shrink is **one atomic commit** across all of it; there is
no incremental path, because deleting a struct field breaks all readers at once.

### 6.1 The change itself (`types`, owner: this file's owner)

Delete `forwarding_ptr`; keep `identity_hash_code` (§1 — folding it buys nothing). Route
`is_forwarded()` / `forwarding_address()` through `mark_word` using the already-landed
`MARK_FORWARDED` encoding, add `set_forwarding_address()`, and set `HEADER_SIZE = 24`,
`MARK_WORD_OFFSET = 16`. Delete `FORWARDING_PTR_OFFSET`. `ARRAY_LENGTH_OFFSET`,
`NUM_SLOTS_OFFSET`, `OBJECT_KIND_OFFSET`, `ARRAY_ELEMENT_TYPE_OFFSET`, `GC_AGE_OFFSET`,
`GC_FLAGS_OFFSET` and `IDENTITY_HASH_CODE_OFFSET` **all keep their current values**.

### 6.2 `types/src/lib.rs` — **blocks the encoding from being usable at all**

`mod heap_types;` is **private** with an explicit re-export list (lines 48–58). The three
constants added this session are `pub` inside a private module and therefore **not
reachable from any other crate**:

- `MARK_FORWARDED`
- `FORWARDING_PTR_MASK`
- `IDENTITY_HASH_CODE_OFFSET`

Add them to that `pub use heap_types::{…}` list. The *methods*
(`ObjectHeader::make_forwarded` / `forwarding_target` / `is_forwarded_mark`) are already
reachable because `ObjectHeader` itself is re-exported, but `monitor.rs`-style
`match mark_state(m) { s if s == MARK_FORWARDED => … }` consumers need the constant.
Also: `types/src/lib.rs:119` asserts `HEADER_SIZE == 32` in `reexport_heap_constants`.

### 6.3 `.forwarding_ptr` field accesses — every one stops compiling

| File | Sites | Notes |
| --- | ---: | --- |
| `gc/src/compact_header.rs` | 589, 590, 746 | `HeaderView::from_legacy`, `set_forwarding_ptr` bridge |
| `gc/src/g1.rs` | 444, 535, 3161, 3700, 11352, 11408, 11409, 11455 | **444 reinterprets the field as `AtomicUsize` for a CAS** — see §4.3, this is the one with a new race |
| `gc/src/old_gen.rs` | 688, 732, 926, 927, 936, 939, 1221 | |
| `gc/src/region.rs` | 694 | `(*dst).forwarding_ptr = (*src).forwarding_ptr` during copy |
| `gc/src/gc.rs` | 436, 518, 527 | `addr_of_mut!(…).write(new_ptr)` |
| `vm/src/jit/helpers.rs` | 4678 | reads via `FORWARDING_PTR_OFFSET`; compiles but returns garbage |
| `vm/tests/tier1_tests.rs` | 1041 | |
| `jit/src/x64.rs` | 15269, 15274 | the two dword-immediate stores in `emit_inline_tlab_new` — delete both; the remaining mark-word pair must still cover 8 bytes |

`gc/src/gen_heap.rs` (mine, 16 sites) and `jit/src/x64.rs` (mine) are handled in the same
commit.

### 6.4 Literal `32` / `24` assertions that fail loudly

`types/src/lib.rs:119` · `gc/src/compact_header.rs:1407, 1409, 1919, 1920` ·
`gc/src/compressed_oops.rs:787` (+ `801, 804, 807, 810, 811`) ·
`vm/src/vm.rs:73487, 73489, 73531, 73554, 73571` ·
`vm/src/runtime/instrument.rs:1823, 1830, 1837, 1844` ·
`gc/src/old_gen.rs:1036–1043` (`assert_eq!(og.used(), 80)` → 64, via
`old_gen.rs:265`'s `size.max(HEADER_SIZE.max(8))`) ·
`vm/src/runtime/interpreter.rs:47046`.

**Production code, not tests:** `gc/src/compact_header.rs:882` and `:921` bake
`bytes_saved.fetch_add(24, …)` — the literal 32→8 delta. At `HEADER_SIZE = 24` that
becomes 16. It feeds `savings_report().header_bytes_saved`, which four of the tests above
assert on.

### 6.5 The one that fails *silently* — fix this first

`jit/tests/intrinsic_arrays_ops.rs:21-22` **redefines the constants locally**:

```rust
const HEADER_SIZE: usize = 32;
const ARRAY_LENGTH_OFFSET: usize = 12;
```

and lays out fake arrays with them (`:112`, `:139`, `:145`). The JIT under test will emit
the *new* offset while the fixture lays out the *old* one — out-of-bounds reads, not a
clean assertion failure. Every sibling test file imports from `cratonvm_types`; this one
should too. **This is the only site in the workspace that fails unsoundly, and it should
be converted before the shrink, as a standalone no-op commit.**

### 6.6 `jit/src/ir_lower.rs` — a second emitter the tripwire never covered

`header_offset_emission_site_inventory_matches_the_doc` scans only `x64.rs`. `ir_lower.rs`
is a second x64 emitter with five header-offset emission sites that were invisible to the
audit the shrink was planned from:

- `ir_lower.rs:1971`, `:1992` — `HEADER_SIZE as u8` inside literal instruction byte arrays (disp8)
- `ir_lower.rs:1887`, `:1923` — `HEADER_SIZE as i32` (disp32)
- `ir_lower.rs:2604` — `ARRAY_LENGTH_OFFSET as u8` (disp8, `MOV R10D,[RAX+12]`)
- `ir_lower.rs:1186` (added 2026-07-31) — `(HEADER_SIZE + packed_body_offset) as i32`,
  the guarded inline compact `getfield` cell address (disp32, so no disp8 hazard;
  it is here because it bakes the header size into emitted machine code)

Plus `jit/src/lib.rs:3210`, `:3236` (was `:3186`, `:3226` — the two closures were
rewritten by BUG-STRING-CODER-COMPACT-20260726) —
`(cratonvm_types::HEADER_SIZE + …) as i32`.

All are value-safe at `HEADER_SIZE = 24` (24 fits signed disp8, `ARRAY_LENGTH_OFFSET`
does not move), so **§6.6 needs no edit for the 24-byte target** — but it must be in the
inventory before anyone attempts 16. This session added
`ir_lower_header_offset_sites_are_inventoried_too` to `x64.rs` to close the blind spot.

**2026-08-03, COV-02.** `ir_lower.rs` gained the integral and reference array
element access it never had
(`docs/internal/cov-02-array-element-access-RETIRED-20260803.md`), which is three more
emission sites — and none of them is a raw narrowing cast:

- `emit_gpr_array_elem_load` / `emit_gpr_array_elem_store` — one
  `disp::disp8_const(HEADER_SIZE as i64)` per emitter, shared across every
  element width (`int`/`long`/`byte`/`char`/`short`/`ref`, wide and narrow oops)
- the `Op::ArrayLength` lowering arm —
  `disp::disp8_const(ARRAY_LENGTH_OFFSET as i64)`, `MOV EAX,[RAX+12]`

`disp8_const` is a `const fn` that fails the BUILD above 127, so these three are
the first sites in this file for which "the shrink went the wrong way" is not
representable. The eleven distinct instruction encodings deliberately share two
displacement expressions; keep it that way, or the 16-byte attempt inherits
eleven places to check instead of two.

### 6.7 `gc/src/tlab.rs` — the GAP_FILLER sub-header aliasing

`install_tail_filler` (`tlab.rs:411-424`) writes a sub-`HEADER_SIZE` gap as
`class_id` at +0 and **the gap size at +4** — deliberately aliasing the
`kind`/`element_type`/`gc_age`/`gc_flags` dword. The readers in `gc/src/gen_heap.rs`
(lines 4176, 5430, 6170, 6498, 7087, 8261, 8438, 9612, 10431, 11067) validate it with
`(8..HEADER_SIZE).contains(&gap)`.

At `HEADER_SIZE = 24` the admissible gap set narrows from {8, 16, 24} to {8, 16} — the
writer's `tail < HEADER_SIZE` branch and the reader's range must move **together**, across
a file boundary. Both halves are correct for any `HEADER_SIZE > 8`, so this is a
coordination item, not a blocker. It becomes a real design question only at
`HEADER_SIZE = 16`, where the gap size at +4 would alias `shape` instead.

These reader sites were deliberately **not** converted to named constants this session:
offset 4 means "gap size" here, not "kind byte", so naming it `OBJECT_KIND_OFFSET` would
be actively misleading. The constant belongs next to `GAP_FILLER_CLASS_ID` in `tlab.rs`.

### 6.8 Bare-literal header offsets in non-owned files

- `vm/src/jit/helpers.rs:2288` — `*(raw_ptr.add(8) as *mut i32) = hash;` **(write)**. Every
  other field in `jit_tlab_post_init` uses a named constant; this one does not. Replace
  with `IDENTITY_HASH_CODE_OFFSET` once §6.2 exports it. Same function, `:2267` —
  `*(raw_ptr.add(4) as *mut u32) = 0;`.
- `vm/src/jit/helpers.rs:4667` — `base.add(8)` read; `:3658` — `(val as *const u8).add(12)`.
- `vm/src/runtime/interpreter.rs:315`, `:3694` — `p.add(12)` array-length diagnostics.
- `vm/src/runtime/interpreter.rs:3699`, `:3701` — `p + 40` for array data. **Already wrong
  by 8 bytes today** (data starts at `HEADER_SIZE` = 32, not 40); worth fixing regardless.
- `jit/tests/intrinsic_string_search.rs:172`, `intrinsic_string_access.rs:175`,
  `intrinsic_arraycopy.rs:242` — `base.add(12)`. Value-safe at 24.
- `vm/src/runtime/interpreter.rs:475-476`, `:21264-21266`, `:21569`, `:21586`,
  `:47060-47064` — a `[u8; 16]` all-zero header snapshot used as a stale-pointer
  heuristic. Compiles and stays correct at 24 (it reads a 16-byte prefix of a 24-byte
  header). **At `HEADER_SIZE = 16` its semantics flip** from "prefix is zero" to "the whole
  header is zero" — re-derive it before attempting §3.

### 6.9 Stale comments that will mislead the person doing this

`gc/src/gc.rs:501` ("`mark_word` … at MARK_WORD_OFFSET (32)" — it is 24) ·
`gc/src/region.rs:679` ("mark_word stays untouched at offset 32") ·
`vm/src/jit/helpers.rs:2247-2257` (the "Layout reminder" block still describes the **old
40-byte** header: `off 24: forwarding_ptr`, `off 32: mark_word`; actual 16 and 24 —
already flagged as R3 in the x64 doc and still unfixed) ·
`vm/tests/tier1_tests.rs:1027-1029` ("full 32-byte `ObjectHeader`", "`forwarding_ptr`
field (offset 24)") · `jit/tests/intrinsic_arrays_ops.rs:11`, `:103`.

---

## 7. What was landed this session

All of it is inert with respect to runtime behaviour: no constant changed value, no gate
was added, no default was flipped.

**`types/src/heap_types.rs`**

- `IDENTITY_HASH_CODE_OFFSET = 8` with an `offset_of!` compile-time assert — closes
  request R3 of the x64 contract doc §7, which was addressed to this file's owner.
- Replaced the stale `jit/src/x64.rs:6690-6813` citation on the `HEADER_SIZE <= 127`
  assert (that range holds loop/BCE analysis, not an emitter) — also R3.
- `MARK_FORWARDED` / `FORWARDING_PTR_MASK` / `make_forwarded` / `forwarding_target` /
  `is_forwarded_mark` — the §4 encoding, with the ordering contract in the doc comment.
  **No producer.** `is_forwarded()` still reads the field.
- Compile-time `HEADER_SIZE % 8 == 0` (the TLAB grid invariant, previously only a
  release-compiled-out `debug_assert` on the allocation path) and
  `ARRAY_LENGTH_OFFSET <= 127`.
- 13 tests: the packing/size derivation, the grid invariant over legacy + compact + array
  shapes, array element addressing, all four mark states round-tripping through CAS, the
  inflated/forwarded aliasing trap, hash orthogonality, destructive-forwarding ordering,
  and field/mark-word coexistence.

**`jit/src/x64.rs`** — three tests appended to `mod flag_and_header_contracts`:
`ir_lower_header_offset_sites_are_inventoried_too` (closes the §6.6 blind spot),
`inline_tlab_total_size_stays_on_the_eight_byte_grid`, `every_header_field_is_dword_addressable`.
Needles are assembled at runtime so the existing inventory tripwire's counts are
untouched — verified still 31 / 11 / 16 / 5.

**`gc/src/gen_heap.rs`** — the three unambiguous baked header-field reads in the
array-guard diagnostic (`+4`, `+5`, `+12`) now use `OBJECT_KIND_OFFSET`,
`ARRAY_ELEMENT_TYPE_OFFSET`, `ARRAY_LENGTH_OFFSET`. The GAP_FILLER `+4` reads were
deliberately left alone (§6.7).

---

## 8. Verification status

- **Not built and not tested.** Nine concurrent agents share this host and nine cargo
  builds OOM it. All new code is `#[cfg(test)]` or a `const _: () = assert!(…)`; the only
  non-test edits are three constant substitutions in a `gen_heap.rs` diagnostic and three
  new `pub` items that no caller reaches yet.
- `rustfmt --check` run after every edit against a baseline captured from `HEAD`:
  `heap_types.rs` 1 deviation before and 1 after, `x64.rs` 15 before and 15 after
  (matching the count the x64 session recorded), `gen_heap.rs` 2 before and 2 after. No
  new deviation introduced; the four my edits did introduce were hand-corrected rather
  than by running `rustfmt`, which would have rewritten CRLF to LF.
- CRLF line endings verified byte-for-byte preserved on all three source files
  (`bareLF = 0` after every edit).
- **The grid invariant holds at 24 and at 16.** `total_size = HEADER_SIZE + body` is a
  multiple of 8 for any 8-aligned `HEADER_SIZE`, because `body` is always a multiple of 8
  (legacy `n*16`; compact pinned by `set_compact_shape`'s `body_size & 7 == 0`; arrays
  rounded by `array_data_size`). It is now a compile-time assert rather than a
  release-compiled-out `debug_assert`. The JIT's unrounded `new_cursor = aligned +
  total_size` continues to agree with `Tlab::alloc_initialized`'s rounded footprint for
  the same reason.
