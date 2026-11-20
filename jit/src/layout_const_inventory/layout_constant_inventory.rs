/// Every object-layout constant owned by `cratonvm_types` that this crate
/// could plausibly bake into an instruction encoding.
///
/// Round 11 wave 15 (lane hdr), two changes for the 8-byte header:
///
/// * The sixth entry was `IDENTITY_HASH_CODE_OFFSET`, a constant that no
///   longer exists (the hash word is `IDENTITY_HASH_OFFSET` since the
///   2026-08 shrink), so that column counted a name nothing can spell and
///   its zeros were vacuous -- while `x64/objects.rs` emitted the real one
///   three times, unrecorded. It now names the real constant.
/// * `NUM_SLOTS_OFFSET` is appended. Since the 8-byte header it is the
///   hazardous one: offset 8 is a LONG header's shape word but a COMPACT
///   instance's first field, so every emitted read or write of it must be
///   dominated by a `GC_FLAG_COMPACT` test (or sit on an allocator's legacy
///   arm). A new use has to be looked at, which is what counting it buys.
const LAYOUT_CONSTANTS: [&str; 9] = [
    "HEADER_SIZE",
    "ARRAY_LENGTH_OFFSET",
    "SLOT_SIZE",
    "REF_ELEMENT_SIZE",
    "MARK_WORD_OFFSET",
    "IDENTITY_HASH_OFFSET",
    "FIELD_CELL_PAYLOAD32_OFFSET",
    "FIELD_CELL_PAYLOAD64_OFFSET",
    "NUM_SLOTS_OFFSET",
];

/// `(file, counts)` where `counts[i]` is the number of code uses of
/// `LAYOUT_CONSTANTS[i]` in that file.
const INVENTORY: [(&str, [usize; 9]); 3] = [
    // lib.rs: the `use` list near the top, plus `StringFieldLayout::new`'s
    // two offset closures — `legacy()` (header-plus-cell, then the ref or
    // int-category payload offset inside that cell: one use of each) and
    // `compact()` (header-plus-body-offset, no payload bias at all, since
    // a `CompactLayout` offset already IS the payload address). Before
    // BUG-STRING-CODER-COMPACT-20260726 this read `[…, 0, 2]`: the single
    // `cell()` closure it replaced biased BOTH branches by payload64 and
    // never mentioned payload32, which is precisely how the compact arm
    // ended up 4 bytes past `coder` and `hash`.
    //
    // 2026-08-12: `AtomicIntFieldLayout::new` (the ATOMIC_INT intrinsic
    // region) adds the same two-offsets-per-field pair as the String one,
    // for `AtomicInteger.value`: `HEADER_SIZE` 3 -> 5, `SLOT_SIZE` 2 -> 3
    // and `FIELD_CELL_PAYLOAD32_OFFSET` 1 -> 2 (the legacy arm's
    // header-plus-cell-plus-payload32 address), and `HEADER_SIZE` again
    // for the compact arm's header-plus-body-offset. `value` is an `int`,
    // so there is no payload64 arm and no ref/narrow-oop case. Both sites
    // are disp32 in the emitted `LOCK XADD [RAX+disp32], ECX`, so neither
    // shares the disp8 hazard.
    //
    // 2026-08-28: `AtomicLongFieldLayout::new` (the ATOMIC_LONG region)
    // is the 64-bit twin and adds the same pair again, for
    // `AtomicLong.value`: `HEADER_SIZE` 5 -> 7 (the legacy
    // header-plus-cell address and the compact header-plus-body one),
    // `SLOT_SIZE` 3 -> 4 (the legacy cell index), and
    // `FIELD_CELL_PAYLOAD64_OFFSET` 1 -> 2 (the legacy payload bias).
    // `FIELD_CELL_PAYLOAD32_OFFSET` does NOT move: `value` is a `long`,
    // so it biases by the 64-bit payload offset, and pointing it at the
    // 32-bit one would read four bytes of the cell's TAG along with half
    // the value -- which is why `AtomicLongFieldLayout::new` also refuses
    // a compact storage width that is not exactly 8.
    //
    // Value-safety at a shrunk header, which is what this inventory
    // exists to make someone check: both addresses are emitted as the
    // disp32 of `MOV RCX, [RAX+disp32]` (`48 8B 88`) or
    // `LOCK XADD [RAX+disp32], RCX` (`F0 48 0F C1 88`) -- ModRM mod=10,
    // a full signed 32-bit displacement. Neither shares the disp8
    // backwards-addressing hazard the `ir_lower.rs` array sites have, and
    // a smaller `HEADER_SIZE` simply makes both numbers smaller. As for
    // its 32-bit twin, the codegen picks between the two per OBJECT on
    // the `GC_FLAG_COMPACT` header bit, so a shrink must move BOTH or the
    // legacy arm reads the wrong cell.
    //
    // 2026-09-02: the String-access compact rows installed after
    // `IrBuilder::build` in `try_compile_inner` add ONE `HEADER_SIZE`,
    // 7 -> 8. Nothing else on this list moves: no new `SLOT_SIZE`, no
    // new payload bias, no new array constant.
    //
    // Value-safety at a shrunk header, which is what this inventory
    // exists to make someone check: **this site emits no displacement
    // at all.** It subtracts `HEADER_SIZE` from
    // `StringFieldLayout::value_compact_offset` /
    // `coder_compact_offset` to recover the CompactLayout body offset,
    // which `ir_lower::emit_inline_compact_getfield` then re-adds as
    // `HEADER_SIZE + c_off` before encoding it. The header size cancels
    // exactly, so the emitted disp32 is the same number at any header
    // size and the round trip cannot go stale against a shrink. It is
    // counted here anyway because a use that CANNOT be wrong today is
    // still a use that a later edit can make wrong, which is the
    // premise of counting every occurrence rather than every hazard.
    //
    // 2026-09-11: `StringBuilderFieldLayout::new` (the
    // STRINGBUILDER_ACCESS region) adds the same two-offsets-per-field
    // pair the three layouts above it add, and it carries its own copy of
    // both closures, so it moves every constant those two spell:
    // `HEADER_SIZE` 8 -> 10 (the `legacy()` header-plus-cell address and
    // the `compact()` header-plus-body one), `SLOT_SIZE` 4 -> 5 (the
    // legacy cell index), and BOTH payload biases 2 -> 3 — `value` is a
    // reference and so biases by payload64, while `count` and `coder` are
    // int-category and bias by payload32. The layout names no array
    // constant: `value.length` is read in the emitter, not here.
    //
    // Value-safety at a shrunk header, which is what this inventory
    // exists to make someone check: every address this layout produces is
    // emitted as the disp32 of a `MOV` (`48 8B 90` / `44 8B 80` /
    // `44 0F B6 88`) or of the `MOV [RAX+disp32], R8D` count store —
    // ModRM mod=10, a full signed 32-bit displacement — so none of them
    // shares the disp8 backwards-addressing hazard the `ir_lower.rs`
    // array sites have, and a smaller `HEADER_SIZE` simply makes every
    // number smaller. The codegen picks between the compact and legacy
    // address per OBJECT on the `GC_FLAG_COMPACT` header bit, so a shrink
    // must move BOTH or the legacy arm writes `count` into the wrong cell.
    //
    // 2026-09-24 (8-byte compact header): a compact layout now hands out
    // ABSOLUTE displacements, so every `compact()` arm and the String-row
    // round trip lose their `HEADER_SIZE` -- 10 -> 5. What remains is the
    // `use` list and the four `legacy()` header-plus-cell addresses, which
    // keep the 16-byte long header.
    //
    // 2026-09-25 (round 11 wave 15, lane hdr): the sixth column now names
    // `IDENTITY_HASH_OFFSET` and a ninth, `NUM_SLOTS_OFFSET`, is appended
    // (see `LAYOUT_CONSTANTS`). This file spells neither: 0 and 0.
    ("lib.rs", [5, 1, 5, 1, 0, 0, 3, 3, 0]),
    // 2026-10-01 (gcf f2/p): 17 -> 18, the unit test
    // `gcf_f2p_ir_card_store_marks_a_clean_card_inline_under_the_mirror`
    // computes an element address `arr + HEADER_SIZE` in Rust to check the
    // card the inline store marked. A test, not an emitted site.
    //
    // ir_lower.rs: the `use` list, the three compile-time invariants
    // restated at the top of that file, two disp32 field-address
    // computations, two disp8 float array element accesses, and the disp8
    // array-length load that guards every bounds check. The eighth
    // `HEADER_SIZE` is the guarded inline compact `getfield`'s cell
    // address (`HEADER_SIZE + packed_body_offset`) — a disp32 site, so it
    // does not share the disp8 backwards-addressing hazard, but it does
    // bake the header size into machine code.
    //
    // COV-02 added two more of each of the first two. `HEADER_SIZE`
    // 8 -> 10: `emit_gpr_array_elem_load` and `emit_gpr_array_elem_store`,
    // one shared displacement apiece covering every integral/reference
    // element width (int, long, byte, char, short, ref — wide and narrow).
    // That is deliberately ONE site per emitter rather than one per width;
    // the header shrink has fewer places to visit, and both go through
    // `disp::disp8_const`, so an oversized header is a build failure rather
    // than a read before the object. `ARRAY_LENGTH_OFFSET` 3 -> 4: the
    // `arraylength` lowering's own length load, alongside the bounds
    // check's.
    //
    // cov-01 added one site on top of that, `emit_inline_getstatic`, which
    // accounts for the fifth `SLOT_SIZE`, the fourth
    // `FIELD_CELL_PAYLOAD32_OFFSET` and both `FIELD_CELL_PAYLOAD64_OFFSET`s
    // (the `use` list and the site). It addresses a STATICS block, which
    // has no object header — hence no new `HEADER_SIZE` — and reaches the
    // cell as `field_index * SLOT_SIZE + payload_offset` from the block
    // base, the same 16-byte `Value` cell shape
    // `field_cell_layout_matches_value_enum` pins. It is a disp32 site
    // (`48 8B 80 disp32` / `48 63 80 disp32`), so it does not share the
    // disp8 backwards-addressing hazard the three array sites have.
    //
    // 2026-08-18 added the eleventh `HEADER_SIZE`, the sixth `SLOT_SIZE`,
    // and TWO each of `FIELD_CELL_PAYLOAD32_OFFSET` (4 -> 6, the `F` arm
    // and the int-category default arm) and `FIELD_CELL_PAYLOAD64_OFFSET`
    // (2 -> 4, the reference arm and the `J`/`D` arm): the IR inline
    // `getfield`'s LEGACY
    // branch, `HEADER_SIZE + field_index * SLOT_SIZE` plus the payload bias
    // inside the 16-byte `Value` cell. It is the arm that stopped every
    // legacy-layout receiver from taking `jit_getfield` — see
    // every-jit-getfield-takes-the-helper-FIXED-20260820.md
    // — and it is a disp32 site in all three forms it emits
    // (`48 8B 80 disp32`, `8B 80 disp32`, `48 63 80 disp32`), so it does
    // not share the disp8 hazard either. `SLOT_SIZE` goes 5 -> 6 with it:
    // the legacy cell address is `field_index * SLOT_SIZE`, a use of its
    // own and not a reuse of the compact arm's — which this comment claimed
    // until the inventory test refused the count and said so.
    //
    // 2026-09-02 added the twelfth `HEADER_SIZE` and two more in tests
    // (12 -> 14): the optimizing tier's GATED compact reference
    // `putfield` (`emit_gated_ir_ref_putfield`) and the two test
    // expectations that reconstruct the same address to assert the store
    // is emitted at it. The emitter site is the mirror image of the
    // inline `getfield` compact read directly above it — same
    // `HEADER_SIZE + packed_body_offset`, same cell — and it is a disp32
    // site (`48 89 90 disp32`), so it does not share the disp8
    // backwards-addressing hazard the array sites have. Nothing else
    // moves: the store reaches the compact cell base directly, with no
    // `SLOT_SIZE` index and no payload bias, because a compact reference
    // field IS the bare 8-byte pointer.
    //
    // Later the same day, the gated store grew its LEGACY shape and the
    // counts moved again: `HEADER_SIZE` 14 -> 15, `SLOT_SIZE` 6 -> 7 and
    // `FIELD_CELL_PAYLOAD64_OFFSET` 4 -> 5, all three from the one
    // expression `HEADER_SIZE + field_index * SLOT_SIZE` plus the payload
    // bias inside the 16-byte `Value` cell. It exists because the compact
    // shape alone fired zero times out of 16,384,000 -- the TLAB fast path
    // writes legacy headers unconditionally -- and it is the exact mirror
    // of the inline `getfield`'s own legacy branch two entries above, which
    // is where the offsets are transcribed from rather than re-derived.
    // Both stores are disp32 (`48 89 90 disp32`, `4C 89 90 disp32`), so
    // neither shares the disp8 backwards-addressing hazard; and since the
    // arm now picks between the two shapes per OBJECT on the
    // `GC_FLAG_COMPACT` bit, a header shrink must move BOTH or the legacy
    // shape writes the wrong cell.
    // Plus the two-shape test's own expectations (`HEADER_SIZE` 15 -> 17):
    // it reconstructs both cell addresses to assert both stores are
    // emitted, which is the assertion that would have caught the
    // compact-only arm before a run-time census had to.
    //
    // 2026-09-04: the layout-epoch guard's regression test adds three more
    // `HEADER_SIZE` uses (17 -> 20), all of them reading back the compact
    // cell it just proved is or is not written. No new EMISSION site: the
    // guard itself bakes an epoch address and a count, not a displacement.
    //
    // 2026-09-19 (round 9 wave 9, irl9): -> [21, 6, 8, 1, 0, 0, 7, 7]. The IR
    // tier's inline ArrayList `get`/`size` prefix bounds the index by the
    // backing `Object[]`'s length (`ARRAY_LENGTH_OFFSET`, disp8, a live array
    // the allocator stamped) and loads the element at the array data base
    // (`REF_ELEMENT_SIZE` scale); its executed test rebuilds the fake list's
    // field cells (`HEADER_SIZE`, `SLOT_SIZE`, the payload offsets). The
    // `instanceof` fast path reads only the class-id / kind-tag header words
    // through existing helpers. Every new emission is disp8/disp32 behind the
    // same allocator screen as its siblings.
    //
    // 2026-09-23 (round 11 wave 2, lane lower): -> [25, 6, 9, 1, 0, 0, 7, 8].
    // `emit_inline_prim_putfield`, the optimizing tier's inline primitive
    // `putfield`, adds two `HEADER_SIZE` emission sites -- the compact cell
    // `HEADER_SIZE + c_off` and the legacy cell `HEADER_SIZE + field_index *
    // SLOT_SIZE` (the new `SLOT_SIZE`) -- plus the legacy cell's upper word
    // at `FIELD_CELL_PAYLOAD64_OFFSET` (the new payload64). All three are
    // disp32 and built with `checked_add`/`checked_mul`/`i32::try_from`, so a
    // header that no longer fits refuses the inline form instead of wrapping
    // a displacement. It picks its shape per OBJECT on `GC_FLAG_COMPACT`,
    // like the gated reference store above, so a shrink must move both. The
    // other two `HEADER_SIZE` uses are its unit test's expected bytes.
    //
    // 2026-09-24 (8-byte compact header): the two compact field sites
    // (`emit_inline_compact_getfield` and the gated reference putfield) now
    // bake the layout's ABSOLUTE displacement with no `HEADER_SIZE`, and
    // the compact-store tests assert that displacement directly -- 21 -> 13.
    //
    // 2026-09-25 (round 11 merged with the 8-byte header): -> [15, 6, 9, 1, 0, 0, 7, 8].
    // Round 11's `emit_inline_prim_putfield` (above) on dev's header: its
    // compact arm now bakes the absolute displacement (no `HEADER_SIZE`,
    // and its test expectation lost its one), leaving the LEGACY cell's
    // `HEADER_SIZE + field_index * SLOT_SIZE` and its upper word at
    // `FIELD_CELL_PAYLOAD64_OFFSET`, plus the test's legacy tag address.
    //
    // 2026-09-25 (round 11 wave 15, lane hdr): the renamed sixth column
    // (`IDENTITY_HASH_OFFSET`) is 0 here; the new `NUM_SLOTS_OFFSET` column
    // is 4 -- the legacy-arm bounds reads of `emit_gated_ir_ref_putfield`
    // and `emit_inline_prim_putfield`, the ArrayList prefix's legacy-arm
    // `min_num_slots` compare, and that prefix's executed test's fake
    // legacy receiver. All three emitted reads sit behind a
    // `GC_FLAG_COMPACT` test that routes compact receivers elsewhere.
    //
    // 2026-09-28 (round 13 wave 2, lane irhash): -> HEADER_SIZE 17. The IR
    // `aastore` inline card arm (`emit_ir_aastore_card_arm`) bakes the array
    // header as a disp8 through `u8::try_from(HEADER_SIZE)` and refuses
    // the inline form when it would not fit below 0x80; its executed test
    // addresses element 0 at `arr + HEADER_SIZE`.
    // `ARRAY_LENGTH_OFFSET` 6 -> 7 (JIT round 15 wave 6, lane irosr2): the
    // OSR entry stub's ArrayLength recompute, a null-tested
    // `MOV EAX,[RAX+LEN_DISP]` through the checked `disp8_const`.
    // 2026-10-02 (gcg g4/jm): 18 -> 20. The opt-in IR `aastore` post-barrier
    // arm (`CRATONVM_JIT_IR_AASTORE_POST_BARRIER`) bakes the array header as a
    // disp8 through `u8::try_from(HEADER_SIZE)` and refuses the inline form at
    // 0x80 or above, like the card arm above; its unit test addresses element
    // 0 at `arr + HEADER_SIZE` in Rust (a test, not an emitted site).
    // 2026-10-02 (gcg g6/jm): 20 -> 18. That arm and its test are deleted
    // (the card-store form is the default); the two uses went with them.
    // `ARRAY_LENGTH_OFFSET` 7 -> 8 (JIT round 18 close, lane testfix18):
    // dev's ScopedMemoryAccess fold (95d2493f1e) reads a heap array's length
    // (`MOV EAX, [RDX + len]`) after a kind test proves an array, through
    // the checked `disp8_const`. Value-safe at a shrunk header (disp8, a
    // smaller displacement); a grown one past 127 fails the build.
    // (The two 7 -> 8 notes above and below record the same site, landed
    // on both sides of the 2026-10-06 dev merge; counted once.)
    // 2026-10-06: `ARRAY_LENGTH_OFFSET` 7 -> 8. The IR `ScopedMemoryAccess`
    // raw-access fold (`lower_scoped_raw`, 95d2493f1e) bounds-checks a heap
    // primitive array with `MOV EAX,[RDX+len]`; it landed as a raw
    // `as u8` narrowing and now goes through the checked `disp8_const`.
    // 2026-10-06 (round 16 matrix residual, MX16-5): `HEADER_SIZE` 18 -> 19
    // and `ARRAY_LENGTH_OFFSET` 8 -> 9. The vector pre-loop
    // (`emit_vector_preloop`) folds the array header into each step's
    // element displacement (`i8::try_from(HEADER_SIZE as i32 + step*32)`,
    // refused if it does not fit) and bounds-checks each base with
    // `CMP R10D,[base+len]` through the checked `disp8_const` form.
    // 2026-10-07 (nio20 wave 1, lane L2, NIO19-2): `ARRAY_LENGTH_OFFSET`
    // 9 -> 10. The inline `Unsafe` primitive-array accessor prefix
    // (`try_emit_ir_unsafe_access_prefix`) scales a heap array's length
    // (`MOV EDX,[RAX+LEN]`) after a tag-byte test proves a primitive array,
    // through the checked `disp8_const`. Value-safe at a shrunk header (a
    // smaller disp8); a grown one past 127 fails the build. The same prefix
    // takes `Unsafe.arrayBaseOffset` (16) AS the payload start behind a
    // `const` assert that `ARRAY_DATA_OFFSET == 16`.
    // 2026-10-07 (nio20 wave 3, lane L2): `ARRAY_LENGTH_OFFSET` 10 -> 11.
    // The optimizing tier's String / CharSequence search prefix
    // (`try_emit_ir_string_search_prefix`) hand-encodes `[value+8]` (the
    // byte[] length, disp8) and `[value+idx+16]` (the payload, disp8) in
    // its loops, behind ONE `const` assert that `ARRAY_LENGTH_OFFSET == 8
    // && ARRAY_DATA_OFFSET == 16`: not value-safe at another layout by
    // construction, but a shrink (or growth) fails the BUILD there rather
    // than mis-addressing. Its unit-test fixture layout lives in
    // `crate::tests::synthetic_string_layout`, so it adds no count here.
    // 2026-10-08 (nio20 wave 5, lane L2): `HEADER_SIZE` 19 -> 20,
    // `SLOT_SIZE` 9 -> 10 and `FIELD_CELL_PAYLOAD64_OFFSET` 8 -> 9 from
    // `scoped_ref_field_offsets` (the confined-session fold's `state` /
    // `owner` reads, the 64-bit twin of `AtomicIntFieldLayout::new`, all
    // checked arithmetic); `MARK_WORD_OFFSET` 0 -> 7 and
    // `IDENTITY_HASH_OFFSET` 0 -> 3 from `identity_hash_read_bytes` (the
    // inline identity hash), which emits both as disp8 behind `const`
    // asserts that each is < 0x80, and reads the mark only after the
    // compact / forwarded tests the native decode makes.
    // 2026-10-13 (JIT r20 wave 18 lane xVhArr, AT-2; audited in wave 20
    // lane inv20): `ARRAY_LENGTH_OFFSET` 11 -> 13. Both in
    // `vh_const_array_core_bytes`, the constant-VarHandle `int[]`/`long[]`
    // element arm: the index guard `CMP R11D,[RAX+LEN]` (disp8, through
    // the checked `disp8_const` since wave 20 -- it landed as a raw
    // narrowing beside a `<= 127` assert), and the executed unit test's
    // fake-array fixture writing the length at that offset in Rust (not an
    // emitted site). Value-safe at a shrunk header (a smaller disp8); a
    // grown one past 127 fails the build. Its element offset is a disp32
    // `ARRAY_DATA_OFFSET` pinned to 16 by a `const` assert.
    // JIT r21 waves 3-4: `ARRAY_LENGTH_OFFSET` 13 -> 15, the constant-VarHandle
    // `Object[]` get arm (`vh_const_ref_array_get_bytes`, checked `disp8_const`) and the
    // Latin-1 case prefix's `const` layout assert (lane strings); both value-safe.
    // JIT r21 wave 22 (lane vhconst): `ARRAY_LENGTH_OFFSET` 15 -> 16, the
    // constant-VarHandle `Object[]` STORE arm's index guard
    // (`vh_const_ref_array_store_access_bytes`, `CMP R11D,[RAX+LEN]` through
    // the checked `disp8_const`); value-safe at a shrunk header, a grown one
    // past 127 fails the build.
    // JIT r23 wave 2 (the `Arrays.equals` prefix,
    // `try_emit_ir_arrays_equals_prefix`): `ARRAY_LENGTH_OFFSET` 16 -> 17,
    // its `const` layout assert (`ARRAY_LENGTH_OFFSET == 8 &&
    // ARRAY_DATA_OFFSET == 16`): like the String search prefix, not
    // value-safe at another layout by construction, but a shrink or growth
    // fails the BUILD there rather than mis-addressing.
    // JIT r23 wave 6, lane L5 (the inline reference `System.arraycopy`
    // prefix, `try_emit_ir_ref_arraycopy_prefix`): `ARRAY_LENGTH_OFFSET`
    // 17 -> 18, its `const` layout assert (`ARRAY_LENGTH_OFFSET == 8 &&
    // ARRAY_DATA_OFFSET == 16`), the same build-time guard as the
    // `Arrays.equals` prefix's.
    // JIT round 24 B1 (the vector pre-loop's scoped store): `HEADER_SIZE`
    // 20 -> 22, its base rebase `SUB RAX, HEADER_SIZE` on the heap and the
    // native arm, both through the checked `disp8_const` (a grown header
    // past 127 fails the build; a shrunk one is a smaller disp8).
    // JIT round 21, lane strings (ZCP-4), re-landed 2026-10-26:
    // `ARRAY_LENGTH_OFFSET` 18 -> 19. The `StringLatin1.compareToCI` IR
    // prefix hand-encodes the byte[] length and payload as disp8 behind ONE
    // `const` assert that `ARRAY_LENGTH_OFFSET == 8 && ARRAY_DATA_OFFSET ==
    // 16` (the same build-failing guard as the string search prefix above).
    // Round rx27 wave 3, lane splitfast (2026-10-28): `ARRAY_LENGTH_OFFSET`
    // 19 -> 20. The `ArrayList.add` prefix (`try_emit_ir_arraylist_add_prefix`)
    // reads `elementData.length` through the checked `emit_load_base_disp32`
    // (a disp32 form, so a header change re-addresses it rather than
    // overflowing a disp8).
    // Same prefix, review fix (2026-10-28): `REF_ELEMENT_SIZE` 1 -> 2. The
    // stale-view probe `CMP QWORD [RDX + R11*8 + (DATA - 8)], 0` subtracts
    // one element from the payload start as a disp8 behind the same layout.
    // The allocated primitive and String character loads had two additional
    // header expressions. The three load encoders and the store encoder now
    // share one checked displacement (24 -> 21). The legacy 32-bit payload
    // inventory counts eight sites, including the ArrayList fixture.
    // Register-aware array length adds one checked pattern-table address:
    // ARRAY_LENGTH_OFFSET 20 -> 21. The generalized array guard moves its
    // existing occurrence into the same checked memory-operand encoder.
    // That encoder selects disp8/disp32 and rejects an unencodable offset.
    // The shared legacy primitive load selects one displacement for each
    // payload width. Removing the RAX-only copies takes the 32-bit payload
    // count from eight to six and the 64-bit count from ten to nine.
    // Both loads retain the checked disp32 encoding.
    // FP array load/store now share ARRAY_DATA_DISP8 via emit_fp_array_elem_access (21 -> 19);
    // non-compact primitive Store legacy arm clears high qword at FIELD_CELL_PAYLOAD64_OFFSET (9 -> 10).
    ("ir_lower.rs", [15, 20, 9, 2, 7, 3, 5, 8, 3]),
    // x64/objects.rs, added 2026-09-04. It bakes object-header
    // displacements exactly as the two files above do -- the compact and
    // legacy reference-store cell addresses, the array header, the inline
    // TLAB `new` -- and was covered by NEITHER tripwire: the `x64.rs` scan
    // matches only the `<CONST> as <ty>` cast form, and this inventory
    // listed two files. The gap was found the honest way, by adding a
    // legacy emission site there on 2026-09-02 and having to record it by
    // hand in `header-shrink.md` because nothing counted it.
    //
    // 2026-09-11: `emit_inline_tlab_newarray` and
    // `emit_sb_append_char_body` add the file's first ARRAY constants.
    // `ARRAY_LENGTH_OFFSET` 0 -> 3: the disp8 screen at the top of the
    // array allocator, its `MOV [R11+ARRAY_LENGTH_OFFSET], ECX` shape
    // store, and the `MOV R9D, [RDX+ARRAY_LENGTH_OFFSET]` capacity load in
    // the append body. `MARK_WORD_OFFSET` 2 -> 5: the same screen, plus
    // the array header's two mark-word halves.
    //
    // 2026-09-17: `emit_sb_append_string_copy` (the
    // `append(String)`/`append(int)` in-place copy tail, lane
    // lane-stringbuilder-jit-intrinsic-20260917) adds one more
    // `ARRAY_LENGTH_OFFSET`, 3 -> 4: `MOV ECX,[RDX+ARRAY_LENGTH_OFFSET]`,
    // the destination builder's capacity read, the same shape and the
    // same disp8 form as the append(char) body's own capacity load two
    // sentences up — a live `byte[]` whose header the same constants
    // describe, not a new kind of site.
    //
    // Value-safety at a shrunk header, which is what this inventory exists
    // to make someone check: all six are disp8 sites, which is the
    // hazardous form — so the allocator SCREENS for it, refusing to emit
    // (and keeping the helper) when `MARK_WORD_OFFSET + 4`,
    // `ARRAY_LENGTH_OFFSET` or `ARRAY_DATA_OFFSET` exceeds 127. A GROWN
    // header therefore costs coverage rather than addressing backwards,
    // and a shrunk one only makes the displacements smaller. The three
    // reads outside the allocator itself (the two append bodies' capacity
    // loads) are reached only through that same allocator's screen being
    // satisfiable, and all address a live `byte[]` whose header the same
    // constants describe.
    //
    // 2026-09-24 (gen GC round 4 wave 4, cards4): `HEADER_SIZE` 9 -> 16.
    // ONE new emission site: `emit_ref_element_address`'s
    // `LEA dst, [array + index*ref_element_size + HEADER_SIZE]` (disp8 at
    // today's 16-byte header; `isel::encode_named` picks disp32 for a
    // larger one, so a grown header costs bytes, not correctness). The
    // other six are in `r4w4_cards4_tests` (expected displacements and a
    // test store), not emitted code.
    // 2026-09-24 (8-byte compact header): the two compact reference-store
    // emitters bake the layout's ABSOLUTE displacement (no `HEADER_SIZE`),
    // and `emit_inline_tlab_new` sizes a compact instance from the layout's
    // total and writes the whole mark word as ONE dword -- `HEADER_SIZE`
    // 9 -> 6, and `MARK_WORD_OFFSET` 5 -> 2 (one store per allocator).
    //
    // 2026-09-25 (round 11 wave 15, lane hdr): `IDENTITY_HASH_OFFSET` 0 -> 3
    // -- not new sites, the column finally names the real constant: the
    // object allocator's legacy-arm hash-word store, the array allocator's
    // disp8 screen and its hash-word store. `NUM_SLOTS_OFFSET` = 2: the
    // object allocator's legacy-arm shape store (inside `if
    // compact_body.is_none()`, pinned by
    // `the_long_header_stores_sit_on_the_legacy_arm_of_both_inline_allocators`)
    // and the gated compact reference putfield's legacy-arm bounds read.
    //
    // 2026-10-08 (nio21 wave 10 L1, recorded in wave 13): `ARRAY_LENGTH_OFFSET`
    // 4 -> 5. `emit_zgc_object_end_into_rcx` (the ZGC start-bit-before-
    // commit and helper-before-commit arms of BOTH tiers' inline primitive
    // `newarray`) re-reads the length the same allocator has just stamped,
    // `MOV ECX,[R11+ARRAY_LENGTH_OFFSET]`, to re-derive the array's end. A
    // disp8, screened through `u8::try_from(..)` and `<= 127`: a header
    // past that marks the compile unencodable (no wrong address is ever
    // emitted), and a shrunk one only makes the displacement smaller. It
    // addresses only the array at R11, whose header those same constants
    // describe. (Its `ARRAY_DATA_OFFSET` twin is screened the same way; that
    // constant is not a column here.)
    // zr z9/bo (2026-10-13): +1 `HEADER_SIZE` and +1 `SLOT_SIZE`, in the
    // test `a_real_heap_answers_every_oracle_after_short_announces_across_threads`
    // (it sizes a fake object; nothing is emitted from it).
    // gci i23/lc (2026-10-15): `HEADER_SIZE` 14 -> 21, all seven in the
    // `r4w4_cards4_tests` lean-POST harness and its test
    // (`run_store_lean`, `gci_i23lc_the_lean_post_keeps_every_card_and_spares_the_second_check`):
    // the fake holder's first-field address, the element index / mark
    // offset arithmetic, and the harness's own test store
    // (`i32::try_from(HEADER_SIZE)`, a checked disp32). No production
    // emitter gained a layout constant: the lean PRE / POST and the
    // skip-edge stubs address only the card view and the card byte.
    // zt t11/ca3 (2026-10-19): `MARK_WORD_OFFSET` 2 -> 3 and
    // `NUM_SLOTS_OFFSET` 2 -> 3, the header undo of
    // `CRATONVM_JIT_ZGC_ALLOC_SPILL_SINK`'s divert to `new_object`.
    // zt t12/ca4 (2026-10-19): both 3 -> 2 again. The sink's refusals now
    // call the announce helper on the object as headed, so nothing is
    // undone and the two stores are gone.
    // gcj j2/jt2 (2026-10-19): `HEADER_SIZE` 21 -> 23, both in the
    // `r4w4_cards4_tests` post-only harness and its test
    // (`run_post_only_store`, `gcj2jt2_a_post_only_store_marks_its_card_after_the_store`):
    // the harness's own test store (`i32::try_from(HEADER_SIZE)`, a checked
    // disp32) and the fake holder's first-field address. No production
    // emitter gained a layout constant: the post-only sites only skip their
    // PRE half, and the shift-first card address reads the card view.
    // JIT r22 wave 4 (lane intrinsics, `CRATONVM_JIT_SB_CHARAT_CAPACITY_CHECK`;
    // counted wave 6, lane review): `ARRAY_LENGTH_OFFSET` 5 -> 6. The
    // single-pass `StringBuilder.charAt` body reads the payload `byte[]`'s
    // length, `MOV ECX,[R8+len]`, to bound the index by the array as well as
    // by `count`. A disp8 screened through `u8::try_from(..)` and `<= 127`
    // (a header past that marks the compile unencodable), addressing only the
    // array at R8, whose header the same constant describes.
    // gcj j2/jt2 residual 3: `HEADER_SIZE` 23 -> 26, all three in
    // `r4w4_cards4_tests` (the freeze harness's test store disp32 and the
    // freeze test's holder-field and element addresses), not emitted code.
    ("objects.rs", [6, 6, 3, 0, 2, 3, 2, 2, 2]),
];

fn source(file: &str) -> String {
    match file {
        "lib.rs" => {
            let mut out = include_str!("../lib.rs").to_string();
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib_parts");
            for entry in std::fs::read_dir(&dir).expect("lib_parts").flatten() {
                let p = entry.path();
                if p.extension().is_some_and(|e| e == "rs")
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().ends_with("_tests.rs"))
                {
                    out.push('\n');
                    out.push_str(&std::fs::read_to_string(&p).expect("read lib_part"));
                }
            }
            out
        }
        "ir_lower.rs" => concat!(
            include_str!("../ir_lower/mod.rs"),
            "\n",
            include_str!("../ir_lower/virtual_object_info.rs"),
            "\n",
            include_str!("../ir_lower/mir_plan_and_init.rs"),
            "\n",
            include_str!("../ir_lower/vector_preloop.rs"),
            "\n",
            include_str!("../ir_lower/value_phi_copies.rs"),
            "\n",
            include_str!("../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
            "\n",
            include_str!("../ir_lower/int_expr_bounds.rs"),
            "\n",
            include_str!("../ir_lower/character_to_chars_prefix.rs"),
            "\n",
            include_str!("../ir_lower/getfield_legacy_read.rs"),
            "\n",
            include_str!("../ir_lower/post_call_exit_site_for.rs"),
            "\n",
            include_str!("../ir_lower/entry_fold_node.rs"),
            "\n",
            include_str!("../ir_lower/call_loaded_ic_entry_releasing.rs"),
            "\n",
            include_str!("../ir_lower/fallthrough_poll_spills.rs"),
            "\n",
            include_str!("../ir_lower/ir_ic_cascade_admits_kind.rs"),
            "\n",
            include_str!("../ir_lower/implicit_kind_matches_catch.rs"),
            "\n",
            include_str!("../ir_lower/scoped_raw_tail.rs"),
            "\n",
            include_str!("../ir_lower/frame_needs.rs"),
            "\n",
            include_str!("../ir_lower/back_edge_dead_ref_homes.rs"),
            "\n",
            include_str!("../ir_lower/block_lazy_clear_survivors.rs"),
            "\n",
            include_str!("../ir_lower/promoted_pending_mark.rs"),
            "\n",
            include_str!("../ir_lower/site_value_live_at.rs"),
            "\n",
            include_str!("../ir_lower/plan_register_residency_with.rs"),
            "\n",
            include_str!("../ir_lower/inner_attempt.rs"),
            "\n",
            include_str!("../ir_lower/g1_aastore_screen.rs"),
            "\n",
            include_str!("../ir_lower/scoped_session_confined_rows.rs")
        )
        .to_string(),
        "objects.rs" => include_str!("../x64/objects.rs").to_string(),
        other => panic!("no source registered for {other}"),
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Count whole-identifier occurrences of `ident` in *code*: whole-line and
/// trailing line comments are skipped, double-quoted string literals are
/// skipped, and a match that is part of a longer identifier does not count.
///
/// Deliberately a tokenizer rather than a substring scan, for the reason in
/// the module doc. Two known limitations, both of which can only ever
/// *under*-count and both of which are pinned by the fixed totals above: a
/// `"` inside a character literal makes the rest of that line read as a
/// string, and a string literal continued across a line break is treated as
/// re-opening on the next line.
fn code_occurrences(src: &str, ident: &str) -> usize {
    let mut n = 0usize;
    for raw in src.lines() {
        if raw.trim_start().starts_with("//") {
            continue;
        }
        let bytes = raw.as_bytes();
        let mut in_str = false;
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            if in_str {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    in_str = false;
                }
                i += 1;
                continue;
            }
            if b == b'"' {
                in_str = true;
                i += 1;
                continue;
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                break;
            }
            if is_ident_byte(b) {
                let start = i;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                if &raw[start..i] == ident {
                    n += 1;
                }
                continue;
            }
            i += 1;
        }
    }
    n
}

/// **Verify the counter before trusting anything it reports.**
///
/// More than one "audit" in this repo has turned out to check something
/// other than what its name says, so this pins what `code_occurrences`
/// actually does against a sample whose answer is obvious by inspection —
/// and demonstrates, on the same sample, the blind spot in the substring
/// needle that kept this file out of the header-offset inventory.
#[test]
fn the_counter_counts_what_its_name_says() {
    let sample = concat!(
        "use x::{FAKE_OFF, OTHER};\n",
        "// FAKE_OFF in a whole-line comment must not count\n",
        "/// FAKE_OFF in a doc comment must not count\n",
        "let a = FAKE_OFF + FAKE_OFF;   // FAKE_OFF trailing comment\n",
        "let b = MY_FAKE_OFF + FAKE_OFF_2 + FAKE_OFFSET;\n",
        "panic!(\"FAKE_OFF inside a string literal must not count\");\n",
        "let c = (FAKE_OFF + n) as i32;\n",
    );
    // 1 in the use-list + 2 on the `let a` line + 1 on the `let c` line.
    assert_eq!(
        code_occurrences(sample, "FAKE_OFF"),
        4,
        "the counter must see code uses only, and must never match inside a \
         longer identifier"
    );
    assert_eq!(
        code_occurrences(sample, "NOT_PRESENT_ANYWHERE"),
        0,
        "an absent identifier must count zero, not match spuriously"
    );
    // The substring needle the x64.rs tripwire uses finds NONE of those
    // four, because not one of them is written as a bare constant directly
    // followed by a cast. This is the whole reason a second mechanism is
    // needed rather than another copy of the first.
    assert_eq!(
        sample.matches("FAKE_OFF as i32").count(),
        0,
        "a `<CONST> as <ty>` substring scan sees nothing here even though the \
         constant is used four times, one of them inside a cast expression"
    );
}

/// The inventory itself. Every count, including every zero.
#[test]
fn layout_constant_emission_sites_are_inventoried() {
    for (file, expected) in INVENTORY {
        let src = source(file);
        for (idx, ident) in LAYOUT_CONSTANTS.into_iter().enumerate() {
            let want = expected[idx];
            let found = code_occurrences(&src, ident);
            assert_eq!(
                found, want,
                "{ident} is used {found}x in jit/src/{file}; the header-shrink \
                 inventory records {want}x. Both files emit object-header \
                 displacements into machine code, and neither is covered by \
                 the substring tripwire in x64.rs. Update \
                 layout-constant-hazards.md and \
                 header-shrink.md §6.6 in the same change, and confirm the new \
                 or moved site is value-safe at the new layout — the disp8 \
                 sites in ir_lower.rs silently address backwards past 127."
            );
        }
    }
}

/// The inventory is only meaningful if it is actually reading source. A
/// mistyped `include_str!` path is a compile error, but an empty or
/// truncated read would silently make every count zero and every assertion
/// above pass vacuously for a table of zeros.
#[test]
fn the_inventory_is_reading_real_source() {
    for (file, _) in INVENTORY {
        let src = source(file);
        assert!(
            src.len() > 10_000,
            "{file} read back as {} bytes; the inventory is scanning nothing",
            src.len()
        );
    }
    assert!(
        INVENTORY
            .iter()
            .any(|(_, counts)| counts.iter().any(|c| *c > 0)),
        "an inventory of all zeros would pass without checking anything"
    );
}
