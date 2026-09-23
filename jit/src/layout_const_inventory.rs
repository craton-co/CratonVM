// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Inventory tripwire for the object-layout constants this crate bakes into
//! emitted machine code.
//!
//! Split out of `lib.rs` on 2026-09-17. The boundary is real rather than
//! arithmetic: everything here is ONE tripwire and the prose that justifies
//! its numbers. It reads this crate's own source as text, and nothing outside
//! it calls into the tripwire -- a test that audits the emitters has no reason
//! to live inside the largest emitter.
//!
//! One exception, stated so the sentence above stays true: the
//! `jit_gate_pass` census (`note_jit_gate_pass_hit` / `_fill` /
//! `jit_gate_pass_census`) is runtime state and IS called from outside -- by
//! the interpreter's `execute()` gate and the tiered stats dump. It landed here
//! with the rest of the text that moved out of `lib.rs`, not because it belongs
//! to the tripwire.
//!
//! Glob-re-exported from the crate root, so the move changed no path.
//!
//! # The `use` preamble below
//!
//! Inside `lib.rs` this text sat in the crate root's scope and named `Arc`,
//! `CompiledMethod`, `JitCache` and friends bare. A file module has its own
//! scope, so those become imports. The ones reached through `crate::` rather
//! than a bare path are items that live in SIBLING modules as `pub(crate)`:
//! a `pub use <module>::*` glob re-exports only `pub` items, so a
//! `pub(crate)` sibling is NOT visible at the crate root and `super::NAME`
//! cannot find it. Naming its real module is the fix, not widening it to
//! `pub`.

use std::sync::Arc;

// A glob, deliberately. This text was written in the crate root's scope and
// names dozens of its items bare; enumerating them would be a list that says
// nothing and goes stale the first time a test is added. `crate::*` restores
// exactly the scope the code was written against, and because the crate root
// glob-re-exports the split-out modules, it reaches their `pub` items too.
use crate::*;

// Reached by module path rather than through the glob above: these are
// `pub(crate)` in SIBLING modules, and a `pub use <module>::*` re-export
// carries only `pub` items -- so they are not at the crate root and
// `crate::*` cannot see them. Naming the real module is the fix; widening
// them to `pub` to satisfy a test would not be.
use crate::ea_ir_bridge::narrow_array_value_fits;
use crate::jfr_compile_decision::{note_deferred_new_bail, take_deferred_new_retry};

// ---------------------------------------------------------------------------
// Layout-constant emission inventory (arch-2026-07-26 `layout-constant-hazards`)
// ---------------------------------------------------------------------------

/// Inventory tripwire for every object-layout constant this crate bakes into
/// emitted machine code, covering the two emitters the original header-offset
/// audit did not reach: this file and `ir_lower.rs`.
///
/// # Why this exists alongside the tripwire in `x64.rs`
///
/// `x64.rs::header_offset_emission_site_inventory_matches_the_doc` counts the
/// substring `<CONST> as <ty>` — the constant *immediately* followed by a cast.
/// That needle is exact for `x64.rs`, and its recorded totals were re-verified
/// against the tree on 2026-07-26 (31 / 11 / 16 / 5, all matching), as were the
/// three `ir_lower.rs` totals (2 / 2 / 1). The mechanism does what its name
/// says — but it is *structurally* blind to any site that uses the constant
/// inside a larger expression which is then cast, and that is exactly the shape
/// both of this file's own sites take:
///
/// ```text
/// let abs = (cratonvm_types::HEADER_SIZE + body_off) as i32;
/// (cratonvm_types::HEADER_SIZE + idx * cratonvm_types::SLOT_SIZE) as i32
/// ```
///
/// A substring scan for the cast form reports **zero** hits here. That is the
/// mechanical reason `lib.rs` never appeared in the header-offset inventory
/// even after `x64.rs` and `ir_lower.rs` were audited: the tool could not see
/// it, so no amount of care from the auditor would have.
///
/// This tripwire therefore counts *identifier occurrences in code* instead:
/// every use of the constant, in any expression shape, outside comments and
/// string literals. The zero entries are as load-bearing as the non-zero ones —
/// a constant that starts being used in a file where it never appeared before
/// also trips the assertion and forces the site into the inventory.
#[cfg(test)]
mod layout_constant_inventory {
    /// Every object-layout constant owned by `cratonvm_types` that this crate
    /// could plausibly bake into an instruction encoding.
    const LAYOUT_CONSTANTS: [&str; 8] = [
        "HEADER_SIZE",
        "ARRAY_LENGTH_OFFSET",
        "SLOT_SIZE",
        "REF_ELEMENT_SIZE",
        "MARK_WORD_OFFSET",
        "IDENTITY_HASH_CODE_OFFSET",
        "FIELD_CELL_PAYLOAD32_OFFSET",
        "FIELD_CELL_PAYLOAD64_OFFSET",
    ];

    /// `(file, counts)` where `counts[i]` is the number of code uses of
    /// `LAYOUT_CONSTANTS[i]` in that file.
    const INVENTORY: [(&str, [usize; 8]); 3] = [
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
        ("lib.rs", [10, 1, 5, 1, 0, 0, 3, 3]),
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
        ("ir_lower.rs", [21, 6, 8, 1, 0, 0, 7, 7]),
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
        ("objects.rs", [9, 4, 4, 0, 5, 0, 2, 2]),
    ];

    fn source(file: &str) -> &'static str {
        match file {
            "lib.rs" => include_str!("lib.rs"),
            "ir_lower.rs" => include_str!("ir_lower.rs"),
            "objects.rs" => include_str!("x64/objects.rs"),
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
                let found = code_occurrences(src, ident);
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
}

/// Lifetime and ownership of compiled code and its side tables.
///
/// See `docs/jit/code-cache-lifetime.md`. Every test here is written to be
/// robust against the other tests in this binary running concurrently: nothing
/// asserts an absolute value of a process-global counter, and the install
/// barrier these tests exercise is deliberately per-`JitCache` so that flushing
/// one cache cannot refuse another's publication.
#[cfg(test)]
mod code_cache_lifetime_tests {
    use super::*;

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc executable");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    fn key() -> (Arc<str>, Arc<str>, Arc<str>, cratonvm_types::ClassId) {
        (
            Arc::from("cclt/Subject"),
            Arc::from("m"),
            Arc::from("()V"),
            cratonvm_types::ClassId::new(1),
        )
    }

    /// One VM's publication or redefinition must not flush another VM's
    /// negative memos or inline caches: both counters belong to the cache.
    #[test]
    fn generation_and_redefine_epoch_belong_to_one_cache() {
        let a = JitCache::new();
        let b = JitCache::new();
        assert_eq!(
            a.generation(),
            1,
            "0 must stay the memos' never-probed value"
        );
        let (class, method, desc, cid) = key();
        let b_before = b.generation();
        a.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        assert!(a.generation() > 1, "a publication advances its own cache");
        assert_eq!(b.generation(), b_before, "and no other");

        let (a_epoch, b_epoch) = (a.redefine_epoch(), b.redefine_epoch());
        a.bump_redefine_epoch();
        assert_ne!(a.redefine_epoch(), a_epoch);
        assert_eq!(b.redefine_epoch(), b_epoch);
    }

    /// A compilation that inlined from `Base` and began before
    /// `invalidate_for_class_change("Base")` must not publish afterwards. The
    /// invalidation scanned a cache that did not contain the body yet, so
    /// nothing else would ever withdraw it.
    #[test]
    fn a_body_compiled_before_an_invalidation_of_its_inlined_class_is_refused() {
        let cache = JitCache::new();
        let (class, method, desc, cid) = key();

        let witness = open_compile_epoch_witness();
        let mut stale = ret_body();
        stale
            .inlined_methods
            .push(("cclt/Base".to_string(), "m".to_string(), "()V".to_string()));
        // The hierarchy changes while this compile is still running; nothing
        // published names `cclt/Base` yet, so the scan itself finds nothing.
        assert_eq!(cache.invalidate_for_class_change("cclt/Base"), 0);
        // An invalidation of an unrelated class must not refuse it.
        let mut unrelated = ret_body();
        unrelated.inlined_methods.push((
            "cclt/Other".to_string(),
            "m".to_string(),
            "()V".to_string(),
        ));
        drop(witness);

        cache.put(class.clone(), method.clone(), desc.clone(), cid, stale);
        assert!(
            cache.get(&class, &method, &desc, cid).is_none(),
            "a body compiled against the hierarchy an invalidation retired must not publish"
        );

        let other_method: Arc<str> = Arc::from("other");
        cache.put(
            class.clone(),
            other_method.clone(),
            desc.clone(),
            cid,
            unrelated,
        );
        assert!(
            cache.get(&class, &other_method, &desc, cid).is_some(),
            "an invalidation of a class the body does not depend on must not refuse it"
        );

        // The same dependency compiled AFTER the invalidation publishes.
        let mut fresh = ret_body();
        fresh
            .inlined_methods
            .push(("cclt/Base".to_string(), "m".to_string(), "()V".to_string()));
        cache.put(class.clone(), method.clone(), desc.clone(), cid, fresh);
        assert!(cache.get(&class, &method, &desc, cid).is_some());
    }

    /// `remove` is an invalidation of one key: a compile of that key that began
    /// before the removal must not publish after it.
    #[test]
    fn a_body_compiled_before_its_key_was_removed_is_refused() {
        let cache = JitCache::new();
        let (class, method, desc, cid) = key();
        let witness = open_compile_epoch_witness();
        let stale = ret_body();
        cache.remove(&class, &method, &desc, cid);
        drop(witness);
        cache.put(class.clone(), method.clone(), desc.clone(), cid, stale);
        assert!(cache.get(&class, &method, &desc, cid).is_none());
    }

    /// A callee an invalidation already retired may still be alive — held by
    /// the retirement queue, or here by the test — so the owner pin succeeds.
    /// Baking a call to it must still refuse the caller.
    #[test]
    fn a_retired_direct_callee_is_never_baked_into_a_publication() {
        let cache = JitCache::new();
        let desc: Arc<str> = Arc::from("()V");
        let cid = cratonvm_types::ClassId::new(4731);
        let callee_class: Arc<str> = Arc::from("cclt/RetiredCallee");
        let callee_method: Arc<str> = Arc::from("target");
        cache.put(
            callee_class.clone(),
            callee_method.clone(),
            desc.clone(),
            cid,
            ret_body(),
        );
        let callee = cache
            .get(&callee_class, &callee_method, &desc, cid)
            .expect("callee published");
        cache.remove(&callee_class, &callee_method, &desc, cid);
        assert!(
            callee.retired.load(std::sync::atomic::Ordering::SeqCst),
            "an invalidated body is marked retired"
        );

        let mut caller = ret_body();
        caller
            ._direct_callee_entries
            .push(callee.entry_ptr() as usize);
        caller
            ._direct_callee_expected
            .push((callee.entry_ptr() as usize, callee.artifact_id));
        let caller_class: Arc<str> = Arc::from("cclt/RetiredCaller");
        let caller_method: Arc<str> = Arc::from("call");
        cache.put(
            caller_class.clone(),
            caller_method.clone(),
            desc.clone(),
            cid,
            caller,
        );
        assert!(
            cache
                .get(&caller_class, &caller_method, &desc, cid)
                .is_none(),
            "a caller baking a call to a retired callee must not publish"
        );
        drop(callee);
    }

    /// Read the name registry past a transient `Locked`.
    ///
    /// `lookup_jit_method_name*` is a `try_lock` accessor so it is safe from a
    /// crash handler, which means "the registry was busy" and "no name covers
    /// this address" are different answers that must not be conflated — the
    /// distinction `JitNameLookup` exists for. Same convention as
    /// `live_code_region_covers_a_buffer_the_name_registry_never_saw`.
    fn name_of(addr: usize) -> Option<String> {
        for _ in 0..200 {
            match lookup_jit_method_name_detailed(addr) {
                JitNameLookup::Found(n) => return Some(n),
                JitNameLookup::NotFound => return None,
                JitNameLookup::Locked => std::thread::yield_now(),
            }
        }
        panic!("the name registry stayed locked for 200 attempts");
    }

    /// THE redefinition hole, from the install side.
    ///
    /// `redefineClass` bumps the epoch and flushes the cache, but the
    /// compilation broker stamps nothing on a queued or in-flight request, so a
    /// compile that already read the OLD bytecode completes afterwards. Before
    /// the install barrier, `put` published it unconditionally and the VM then
    /// executed the bytecode the agent had replaced.
    #[test]
    fn a_compilation_that_predates_a_cache_flush_is_not_installed() {
        let cache = JitCache::new();
        let (class, method, desc, cid) = key();

        // A compilation opens, reads bytecode, and produces an artifact...
        let witness = open_compile_epoch_witness();
        let stale = ret_body();

        // ...and a redefinition flushes the cache while it is still in flight.
        cache.clear_all();
        drop(witness);

        cache.put(class.clone(), method.clone(), desc.clone(), cid, stale);
        assert!(
            cache.get(&class, &method, &desc, cid).is_none(),
            "a body compiled against bytecode the flush retired must not be published"
        );
    }

    /// The gate must refuse only what it is meant to: a compilation that STARTS
    /// after the flush publishes normally. Without this the previous test would
    /// also pass on a `put` that never publishes anything.
    #[test]
    fn a_compilation_that_starts_after_a_flush_installs_normally() {
        let cache = JitCache::new();
        let (class, method, desc, cid) = key();

        cache.clear_all();
        let witness = open_compile_epoch_witness();
        let fresh = ret_body();
        drop(witness);

        cache.put(class.clone(), method.clone(), desc.clone(), cid, fresh);
        assert!(
            cache.get(&class, &method, &desc, cid).is_some(),
            "a body compiled after the flush is current and must publish"
        );
    }

    /// The stamp has to be the epoch at compile START, not at buffer finalize —
    /// the whole window the broker cannot see is the one *before* codegen ends.
    ///
    /// Race-free by construction: both artifacts are compared against each
    /// other rather than against a separately-read global, so a concurrent bump
    /// from another test cannot change the outcome.
    #[test]
    fn the_compile_witness_stamps_the_start_epoch_not_the_finalize_epoch() {
        let witness = open_compile_epoch_witness();
        let at_start = ret_body().install_epoch;
        bump_jit_install_epoch();
        bump_jit_install_epoch();
        let at_finalize = ret_body().install_epoch;
        drop(witness);

        assert_eq!(
            at_start, at_finalize,
            "an artifact finalized after a mid-compile epoch bump must still \
             carry the epoch its compilation began at"
        );

        // And with no witness open, the live epoch is used — otherwise a
        // backend entered directly would inherit a stale thread-local.
        let unscoped = ret_body().install_epoch;
        assert!(
            unscoped > at_start,
            "outside a compilation scope the stamp must track the live epoch \
             (got {unscoped}, compile-scope stamp was {at_start})"
        );
    }

    /// `callee_compiler` re-enters `try_compile` on the same thread. The nested
    /// compile must keep the OUTER (older) epoch: it is only useful if the outer
    /// artifact is, and an older stamp can only refuse more.
    #[test]
    fn a_nested_compile_keeps_the_outer_epoch() {
        let outer = open_compile_epoch_witness();
        let outer_stamp = ret_body().install_epoch;
        {
            let inner = open_compile_epoch_witness();
            bump_jit_install_epoch();
            assert_eq!(
                ret_body().install_epoch,
                outer_stamp,
                "a nested compile must not adopt a newer epoch than its caller"
            );
            drop(inner);
        }
        assert_eq!(
            ret_body().install_epoch,
            outer_stamp,
            "closing the nested scope must restore the outer compilation's epoch"
        );
        drop(outer);
    }

    /// The barrier is per-cache on purpose. A global watermark would make one
    /// VM's redefinition refuse another VM's unrelated publication — and, in
    /// this test binary, would make every `clear_all` test a random failure
    /// generator for every concurrent `put` test.
    #[test]
    fn flushing_one_cache_does_not_refuse_another_caches_publication() {
        let flushed = JitCache::new();
        let untouched = JitCache::new();
        let (class, method, desc, cid) = key();

        let witness = open_compile_epoch_witness();
        let body = ret_body();
        flushed.clear_all();
        drop(witness);

        untouched.put(class.clone(), method.clone(), desc.clone(), cid, body);
        assert!(
            untouched.get(&class, &method, &desc, cid).is_some(),
            "a cache that was never flushed must not inherit another cache's barrier"
        );
    }

    /// `JitPICSlot::seed_from_mic` used to read the MIC's entry pointer BEFORE
    /// its owner. `JitMICSlot::clear_compiled_entry` zeroes the entry and only
    /// then takes the owner, so the old order could copy a live raw address into
    /// the PIC together with an owner that had already been released — a
    /// callable pointer with no keep-alive, which the emitted 4-way cascade
    /// `CALL`s. This is that state, written directly.
    #[test]
    fn seeding_a_pic_refuses_a_mic_entry_that_lost_its_owner() {
        use std::sync::atomic::Ordering;

        // A live JIT code region: an unowned address inside one is a compiled
        // body we failed to retain, never a native trampoline.
        let buf = ExecutableBuffer::new(64).expect("alloc executable");
        let orphan = buf.as_ptr() as u64;

        let mic = JitMICSlot::new();
        mic.cached_entry_word
            .store(orphan | JIT_IC_NEEDS_CONTEXT_TAG, Ordering::Release);
        mic.cached_class_id.store(7, Ordering::Release);
        // `compiled_owner` deliberately left empty — the residue of the race.

        let pic = JitPICSlot::new();
        pic.seed_from_mic(&mic, false);

        assert_eq!(
            pic.class_ids[0].load(Ordering::Acquire),
            7,
            "the class guard is still worth carrying forward"
        );
        assert_eq!(
            pic.entry_words[0].load(Ordering::Acquire),
            0,
            "an entry with no live owner must be downgraded to unresolved, not \
             copied into a slot generated code calls without validation — and \
             the ABI flag travels in that word, so it goes with it"
        );
        drop(buf);
    }

    /// An admissible MIC entry still seeds through — the refusal above must not
    /// be "seed_from_mic no longer copies anything".
    #[test]
    fn seeding_a_pic_carries_a_well_formed_mic_entry_forward() {
        use std::sync::atomic::Ordering;

        let mic = JitMICSlot::new();
        // A synthetic non-JIT sentinel: unowned but outside every code region,
        // which is the shape `jit_entry_publishable` admits (native/builtin
        // targets, which nothing unmaps). Same value the sibling PIC tests in
        // `mod tests` use, so it is empirically clear of every real mapping this
        // suite makes.
        let sentinel = 0x7000u64;
        mic.cached_entry_word.store(sentinel, Ordering::Release);
        mic.cached_class_id.store(9, Ordering::Release);

        let pic = JitPICSlot::new();
        pic.seed_from_mic(&mic, false);

        assert_eq!(pic.class_ids[0].load(Ordering::Acquire), 9);
        assert_eq!(pic.entry_words[0].load(Ordering::Acquire), sentinel);
    }

    /// A retired body must stop naming its address. The OS is free to hand that
    /// page to the next `alloc_executable`, and a crash report that names the
    /// dead method is worse than one that names none — it is believed.
    #[test]
    fn a_retired_body_withdraws_its_crash_report_name() {
        let cm = ret_body();
        let entry = cm.entry_ptr() as usize;
        let len = cm.code_len().max(1);
        let name = "cclt/Retired.body()V".to_string();
        register_jit_method_name(entry, len, name.clone());
        assert_eq!(
            name_of(entry).as_deref(),
            Some(name.as_str()),
            "the name must resolve while the body is live"
        );

        drop(cm);
        // `!= our name` rather than `is_none`: the allocator may already have
        // handed this page to a concurrently-running test, which would then
        // legitimately own the address. What must never happen is the DEAD
        // body still answering for it.
        assert_ne!(
            name_of(entry).as_deref(),
            Some(name.as_str()),
            "a dropped body must not keep symbolizing an address the allocator \
             can hand to a different body"
        );
    }

    /// Belt to the withdrawal's braces: if a registration is ever missed, the
    /// newest range for an address must still win, so a recycled address
    /// symbolizes as what is mapped there NOW.
    #[test]
    // x86-64 only: it registers x86-64 compiled bodies by address; the aarch64
    // path publishes different artifacts. Gated so the crate's
    // test run is green on aarch64 rather than carrying known reds.
    #[cfg(target_arch = "x86_64")]
    fn the_newest_registration_for_an_address_wins() {
        let buf = ExecutableBuffer::new(64).expect("alloc executable");
        let entry = buf.as_ptr() as usize;
        register_jit_method_name(entry, 16, "cclt/First.old()V".to_string());
        register_jit_method_name(entry, 16, "cclt/Second.new()V".to_string());
        assert_eq!(
            name_of(entry).as_deref(),
            Some("cclt/Second.new()V"),
            "the most recent publication for an address must be the one reported"
        );
        unregister_jit_method_name(entry);
        assert_ne!(
            name_of(entry).as_deref(),
            Some("cclt/Second.new()V"),
            "withdrawal must remove EVERY range starting at the entry, not just one"
        );
        assert_ne!(
            name_of(entry).as_deref(),
            Some("cclt/First.old()V"),
            "and it must remove the older range for the same entry too"
        );
        drop(buf);
    }

    /// `lookup_jit_code_range` hands out a bare `Arc<CompiledMethod>` inner
    /// address with no keep-alive; the pinning form must hand out a reference
    /// that makes the metadata (and the code buffer) valid for as long as it is
    /// held, even after the cache has released the body.
    #[test]
    fn pinning_a_code_range_owner_retains_the_artifact() {
        let cache = JitCache::new();
        let (class, method, desc, cid) = key();
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        let published = cache.get(&class, &method, &desc, cid).expect("published");
        let entry = published.entry_ptr() as usize;
        register_jit_code_range(
            entry,
            published.code_len().max(1),
            Arc::as_ptr(&published) as usize,
        );

        let pinned = pin_jit_code_range_owner(entry).expect("a live body must pin");
        assert_eq!(pinned.entry_ptr() as usize, entry);
        // A pin is a real strong reference: it survives every other holder
        // releasing, which is exactly what the bare-address form cannot promise.
        drop(published);
        cache.clear_all();
        assert_eq!(
            pinned.entry_ptr() as usize,
            entry,
            "the pinned artifact must stay valid after the cache released it"
        );
        assert_eq!(
            pinned.code_bytes().first().copied(),
            Some(0xC3),
            "and its code buffer must still be mapped and readable"
        );
        drop(pinned);
        unregister_jit_code_range(entry);
    }

    /// A registered range whose body was never published — so nothing in
    /// `jit_entry_owners` can retain it — must pin as `None`. `None` is the safe
    /// answer; a raw address would not be.
    #[test]
    fn pinning_an_unowned_code_range_yields_none() {
        let cm = ret_body();
        let entry = cm.entry_ptr() as usize;
        register_jit_code_range(entry, cm.code_len().max(1), &cm as *const _ as usize);
        assert!(
            pin_jit_code_range_owner(entry).is_none(),
            "a body with no retainable owner must not be handed out as pinned"
        );
        drop(cm);
    }

    /// An address no body covers pins nothing.
    #[test]
    fn pinning_an_unmapped_address_yields_none() {
        assert!(pin_jit_code_range_owner(0).is_none());
        assert!(pin_jit_code_range_owner(0x10).is_none());
    }

    /// The bisect levers' matching rule. Pinned as a pure function because the
    /// live predicate reads two `OnceLock`s seeded from the environment once
    /// per process, which no test can set reproducibly.
    ///
    /// The load-bearing case is the last one. `CRATONVM_DBG=jit-bisect-only`
    /// with a prefix matching NOTHING must force EVERY method interpreted —
    /// that is what makes "allow only X, does the crash survive?" a valid
    /// bisect step. See the retired `annotation-scan-arrayread-sigsegv`
    /// write-up: the OSR path did not consult this predicate at all, so 21 OSR bodies
    /// compiled under exactly that setting and every bisect row read a
    /// meaningless "no effect".
    #[test]
    fn bisect_levers_match_deny_by_substring_and_allow_only_by_prefix() {
        let deny = vec!["org/h2/mvstore/MVStore.commit".to_string()];
        assert!(force_interpret_matches(
            Some(&deny),
            None,
            "org/h2/mvstore/MVStore",
            "commit"
        ));
        assert!(!force_interpret_matches(
            Some(&deny),
            None,
            "org/h2/mvstore/MVStore",
            "rollback"
        ));

        // A bare package entry pins everything under it.
        let pkg = vec!["org/keycloak/".to_string()];
        assert!(force_interpret_matches(
            Some(&pkg),
            None,
            "org/keycloak/Foo",
            "bar"
        ));

        // Allowlist: listed prefix stays eligible, everything else does not.
        let only = vec!["org/apache/tomcat".to_string()];
        assert!(!force_interpret_matches(
            None,
            Some(&only),
            "org/apache/tomcat/util/bcel/classfile/ConstantPool",
            "getConstant"
        ));
        assert!(force_interpret_matches(
            None,
            Some(&only),
            "java/io/BufferedInputStream",
            "read"
        ));

        // Neither lever set: nothing is forced interpreted.
        assert!(!force_interpret_matches(None, None, "any/Class", "any"));

        // A prefix that matches nothing forces EVERY method interpreted.
        let nothing = vec!["zzzNoSuchPrefix".to_string()];
        for (c, m) in [
            ("java/io/BufferedInputStream", "read"),
            (
                "org/apache/tomcat/util/bcel/classfile/ConstantPool",
                "getConstant",
            ),
            ("AnnotationScanSplitProbe", "readBytes"),
        ] {
            assert!(
                force_interpret_matches(None, Some(&nothing), c, m),
                "bisect-only with an unmatched prefix must force {c}.{m} interpreted"
            );
        }
    }
}

/// The `inlined_class_names` early-out must be a shortcut, never a change of
/// answer.
///
/// `invalidate_for_class` runs on EVERY class definition and almost never
/// matches, so it now refuses before scanning when no published body names the
/// class. That is only sound while the set is a true over-approximation of what
/// Reading a devirtualisation target out of a compiled method's own inline
/// cache — the evidence that exists when the receiver PROFILE does not.
///
/// Every case here is a refusal except the last, because the value of this
/// accessor is entirely in what it declines to answer: it feeds a class-id
/// guard, and a guard built on a thrashing cache spends a compare and a branch
/// to reach the ordinary call anyway.
/// Concurrency properties of the inline-cache publication protocol.
///
/// Generated code reads a PIC way with no lock, in a fixed order: the class id,
/// then ONE load of the tagged entry word. These tests model that reader in
/// Rust and race it against every writer: concurrent installs, recompiled
/// targets, `clear_entries`, and invalidation with a retired body.
#[cfg(test)]
mod inline_cache_publication_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Publish `versions` distinct bodies for each of `classes` receivers and
    /// return `(class_id, version) -> entry` plus `entry -> class_id`.
    fn published_bodies(
        cache: &JitCache,
        classes: u32,
        versions: u32,
    ) -> (
        std::collections::HashMap<(u32, u32), u64>,
        std::collections::HashMap<u64, u32>,
    ) {
        let mut by_key = std::collections::HashMap::new();
        let mut class_of = std::collections::HashMap::new();
        for class_id in 1..=classes {
            for version in 0..versions {
                let class: Arc<str> = Arc::from(format!("icpub/C{class_id}").as_str());
                let method: Arc<str> = Arc::from(format!("m{version}").as_str());
                let desc: Arc<str> = Arc::from("()V");
                let cid = cratonvm_types::ClassId::new(class_id);
                let mut buf = ExecutableBuffer::new(64).expect("alloc body");
                buf.emit(&[0xC3]);
                cache.put(
                    class.clone(),
                    method.clone(),
                    desc.clone(),
                    cid,
                    CompiledMethod::new(buf),
                );
                let cm = cache.get(&class, &method, &desc, cid).expect("published");
                let entry = cm.entry_ptr() as u64;
                by_key.insert((class_id, version), entry);
                class_of.insert(entry, class_id);
            }
        }
        (by_key, class_of)
    }

    /// Threads missing on the same cold site with DIFFERENT receivers used to
    /// write the same empty way unreserved, leaving (class A, entry of B) and an
    /// owner that disagreed with the entry. Every way that ends up live must
    /// name one class, that class's entry, and that entry's owner.
    #[test]
    fn concurrent_installs_of_different_receivers_never_mix_a_way() {
        let cache = JitCache::new();
        let (by_key, class_of) = published_bodies(&cache, 4, 1);
        for _round in 0..64 {
            let pic = Arc::new(JitPICSlot::new());
            let start = Arc::new(std::sync::Barrier::new(4));
            let workers: Vec<_> = (1..=4u32)
                .map(|class_id| {
                    let pic = pic.clone();
                    let start = start.clone();
                    let entry = by_key[&(class_id, 0)];
                    std::thread::spawn(move || {
                        start.wait();
                        for _ in 0..16 {
                            pic.install(class_id, "icpub", entry, class_id % 2 == 0, false);
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().expect("installer finishes");
            }
            for class_id in 1..=4u32 {
                let (entry, needs_context) =
                    pic.lookup(class_id).expect("four receivers fit four ways");
                assert_eq!(
                    class_of[&entry], class_id,
                    "lookup({class_id}) returned another class's entry"
                );
                assert_eq!(needs_context, class_id % 2 == 0);
            }
            for i in 0..JIT_PIC_ENTRIES {
                let (class_id, entry, _) = pic.way(i).expect("way exists");
                if !JitPICSlot::is_live_class_id(class_id) {
                    continue;
                }
                assert_eq!(
                    class_of[&entry], class_id,
                    "way {i} pairs a guard with another class's entry"
                );
                let owner = pic.compiled_owners[i]
                    .lock()
                    .as_ref()
                    .map(|o| o.entry_ptr() as u64);
                assert_eq!(
                    owner,
                    Some(entry),
                    "way {i}'s owner must be the body its word names"
                );
            }
        }
    }

    /// Readers that follow generated code's load order — class id, then one
    /// entry-word load, all inside one JIT execution — racing installs of
    /// recompiled targets and `clear_entries`. A reader that matched class C
    /// must never load a word naming a body of any other class: that is the
    /// retarget-under-a-reader the write-once protocol exists to prevent, and
    /// the grace period is what keeps a retired way from being reused under a
    /// reader still between its two loads.
    #[test]
    fn readers_never_pair_a_guard_with_another_receivers_entry() {
        let cache = JitCache::new();
        let (by_key, class_of) = published_bodies(&cache, 6, 3);
        let class_of = Arc::new(class_of);
        let pic = Arc::new(JitPICSlot::new());
        let stop = Arc::new(AtomicBool::new(false));
        // Published so the WRITER can see whether the readers have got any
        // work yet. See the round loop below for why a fixed round count is
        // not enough on a loaded machine.
        let observed = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let pic = pic.clone();
                let stop = stop.clone();
                let class_of = class_of.clone();
                let observed = observed.clone();
                std::thread::spawn(move || {
                    let mut checked = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        let execution = jit_execution_enter();
                        for i in 0..JIT_PIC_ENTRIES {
                            let class_id = pic.class_ids[i].load(Ordering::Acquire);
                            if !JitPICSlot::is_live_class_id(class_id) {
                                continue;
                            }
                            // Widen the window between the two loads.
                            std::hint::spin_loop();
                            let (entry, _) = jit_ic_entry_decode(pic.entry_words[i].load(Ordering::Acquire));
                            if entry == 0 {
                                continue;
                            }
                            assert_eq!(
                                class_of.get(&entry).copied(),
                                Some(class_id),
                                "a reader past way {i}'s guard for class {class_id} loaded another receiver's entry"
                            );
                            checked += 1;
                            observed.fetch_add(1, Ordering::Relaxed);
                        }
                        jit_execution_leave(execution);
                    }
                    checked
                })
            })
            .collect();

        // The writer used to run a FIXED 2,000 rounds. On a machine under load
        // -- a full-workspace `cargo test`, or sixteen copies of this binary at
        // `--test-threads 16`, which is how this surfaced -- all 2,000 can
        // complete before any of the four reader threads is scheduled long
        // enough to see a non-zero entry word. `checked` is then 0, and the
        // assertion below correctly refuses to report `ok` for a run that
        // proved nothing. That is the right verdict and the wrong outcome: it
        // made a genuine race-detection test read as a red build roughly one
        // run in five under contention (measured: 10 of 48).
        //
        // So the writer now runs until the readers have actually observed a
        // published way, with a hard cap. The cap is what keeps this a test:
        // if the protocol ever genuinely starves readers, the loop still ends
        // and the assertion still fires.
        const MIN_ROUNDS: u32 = 2_000;
        const MAX_ROUNDS: u32 = 200_000;
        let mut round = 0u32;
        while round < MAX_ROUNDS && (round < MIN_ROUNDS || observed.load(Ordering::Relaxed) == 0) {
            let class_id = 1 + round % 6;
            let version = round % 3;
            pic.install(
                class_id,
                "icpub",
                by_key[&(class_id, version)],
                false,
                false,
            );
            if round % 97 == 0 {
                pic.clear_entries();
            }
            round += 1;
        }
        stop.store(true, Ordering::Relaxed);
        let checked: u64 = readers
            .into_iter()
            .map(|r| r.join().expect("reader finishes"))
            .sum();
        assert!(
            checked > 0,
            "the readers must actually have raced a published way"
        );
    }

    /// An invalidation marks a body retired and then clears the inline caches;
    /// a helper that resolved the body just before must not leave it installed.
    /// Either the install sees the flag (admission or the post-publication
    /// re-check), or the clearing pass — which takes the same writer lock —
    /// runs after the publication and withdraws it.
    #[test]
    fn a_retired_body_never_survives_a_racing_install() {
        let cache = JitCache::new();
        let (by_key, _) = published_bodies(&cache, 1, 1);
        let entry = by_key[&(1, 0)];
        let owner = resolve_jit_entry_owner(entry as usize).expect("published body is owned");

        for _round in 0..200 {
            owner.retired.store(false, Ordering::SeqCst);
            let pic = Arc::new(JitPICSlot::new());
            let mic = Arc::new(JitMICSlot::new());
            let installer = {
                let (pic, mic) = (pic.clone(), mic.clone());
                std::thread::spawn(move || {
                    for _ in 0..8 {
                        mic.update(1, "icpub", entry, false, false);
                        pic.install(1, "icpub", entry, false, false);
                    }
                })
            };
            // The invalidation's order: retire first, then clear.
            owner.retired.store(true, Ordering::SeqCst);
            let targets: std::collections::HashSet<usize> = [entry as usize].into_iter().collect();
            pic.invalidate_targets(&targets);
            mic.invalidate_target(&targets);
            installer.join().expect("installer finishes");

            assert!(
                pic.lookup(1).map_or(true, |(e, _)| e != entry),
                "a retired body stayed in a PIC way"
            );
            assert!(
                pic.lookup_megamorphic(1).is_none(),
                "a retired body stayed in the hashed table"
            );
            assert_ne!(
                mic.cached_entry().0,
                entry,
                "a retired body stayed in the MIC"
            );
        }
        owner.retired.store(false, Ordering::SeqCst);
    }

    /// The deterministic half: a body already retired is never published.
    #[test]
    fn installing_an_already_retired_body_publishes_nothing() {
        let cache = JitCache::new();
        let (by_key, _) = published_bodies(&cache, 1, 1);
        let entry = by_key[&(1, 0)];
        let owner = resolve_jit_entry_owner(entry as usize).expect("published body is owned");
        owner.retired.store(true, Ordering::SeqCst);

        let mic = JitMICSlot::new();
        mic.update(1, "icpub", entry, true, false);
        assert_eq!(mic.cached_entry(), (0, false));

        let pic = JitPICSlot::new();
        pic.install(1, "icpub", entry, true, false);
        assert_eq!(pic.entries_used(), 0);
        assert!(pic.lookup_megamorphic(1).is_none());
        owner.retired.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod mic_devirt_evidence {
    use super::*;
    use std::sync::atomic::Ordering;

    /// One artifact carrying one MIC slot in a chosen state.
    ///
    /// `entry` defaults non-zero in every helper below that means "installed",
    /// because a zero entry is `prepopulate`'s guard HINT rather than an
    /// observation — see the accessor's doc comment.
    fn artifact_with(bci: usize, class_id: u32, entry: u64, misses: u64) -> CompiledMethod {
        let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
        let slot = Box::new(JitMICSlot::new_at(bci));
        slot.cached_class_id.store(class_id, Ordering::Relaxed);
        slot.cached_entry_word.store(entry, Ordering::Relaxed);
        slot.misses.store(misses, Ordering::Relaxed);
        cm._jit_mic_slots.push(slot);
        cm
    }

    const ENTRY: u64 = 0x4000;

    /// The shape that matters, and the one a statistical bar would have thrown
    /// away: installed once, then served from machine code forever, so `hits`
    /// is 0 and `misses` is 1. This is what `objectsAreEqual` actually looks
    /// like — measured, `bci16:cls1167:h0:m1`.
    #[test]
    fn an_installed_slot_is_evidence_even_with_zero_recorded_hits() {
        let cm = artifact_with(16, 77, ENTRY, 1);
        assert_eq!(cm._jit_mic_slots[0].hits.load(Ordering::Relaxed), 0);
        assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
    }

    /// The bci is the whole point. Before `JitMICSlot::bci` existed the
    /// artifact kept the boxes and threw the pc mapping away, so there was no
    /// way to ask this question at all — every slot looked like every other.
    #[test]
    fn a_slot_for_another_bci_does_not_answer_for_this_one() {
        let cm = artifact_with(16, 77, ENTRY, 1);
        assert_eq!(cm.dominant_receiver_at_bci(20), None);
    }

    /// A class id with no installed entry is `prepopulate`'s seed from PROFILE
    /// data — a guard hint, not an observation. Accepting it would launder a
    /// profile guess back in dressed as runtime evidence, which is exactly the
    /// thing this accessor exists to substitute for.
    #[test]
    fn a_seeded_class_with_no_installed_entry_is_not_evidence() {
        let cm = artifact_with(16, 77, 0, 0);
        assert_eq!(cm.dominant_receiver_at_bci(16), None);
        // The same slot, once something is actually installed, answers.
        let cm = artifact_with(16, 77, ENTRY, 0);
        assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
    }

    /// Misses past the PIC-promotion threshold mean the site has seen
    /// receivers this slot could not serve. That is the one thing these
    /// counters measure honestly — the helper is entered on every one.
    #[test]
    fn misses_past_the_pic_promotion_threshold_are_not_evidence() {
        let cm = artifact_with(16, 77, ENTRY, MIC_TO_PIC_THRESHOLD);
        assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
        let cm = artifact_with(16, 77, ENTRY, MIC_TO_PIC_THRESHOLD + 1);
        assert_eq!(cm.dominant_receiver_at_bci(16), None);
    }

    /// A PIC that has taken a miss at this bci means the adaptive recompiler
    /// already concluded the site is polymorphic. The MIC beside it still holds
    /// whichever receiver it installed FIRST, which is the least informative of
    /// several — guarding on it would be guarding on an accident of ordering.
    #[test]
    fn a_polymorphic_site_is_not_evidence_even_though_its_mic_is_populated() {
        let mut cm = artifact_with(16, 77, ENTRY, 1);
        assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
        let pic = Box::new(JitPICSlot::new_at(16));
        pic.misses.store(5, Ordering::Relaxed);
        cm._jit_pic_slots.push(pic);
        assert_eq!(cm.dominant_receiver_at_bci(16), None);
    }

    /// ...but a PIC at a DIFFERENT bci says nothing about this one, and an
    /// unused PIC (no misses) is not a polymorphism verdict either.
    #[test]
    fn an_unrelated_or_unused_pic_does_not_veto() {
        let mut cm = artifact_with(16, 77, ENTRY, 1);
        let elsewhere = Box::new(JitPICSlot::new_at(99));
        elsewhere.misses.store(5, Ordering::Relaxed);
        cm._jit_pic_slots.push(elsewhere);
        assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
        cm._jit_pic_slots.push(Box::new(JitPICSlot::new_at(16)));
        assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
    }

    /// An unpopulated slot, and one caught mid-publication, are both "no
    /// answer" rather than class 0 / class u32::MAX. The installing sentinel is
    /// the one a concurrent reader can actually observe.
    #[test]
    fn an_empty_or_installing_slot_is_not_evidence() {
        let cm = artifact_with(16, 0, ENTRY, 0);
        assert_eq!(cm.dominant_receiver_at_bci(16), None);
        let cm = artifact_with(16, JitMICSlot::EMPTY_CLASS_ID, ENTRY, 0);
        assert_eq!(cm.dominant_receiver_at_bci(16), None);
        let cm = artifact_with(16, JitMICSlot::INSTALLING_CLASS_ID, ENTRY, 0);
        assert_eq!(cm.dominant_receiver_at_bci(16), None);
    }

    /// A slot with no site — the loop-unroll clones and every test helper —
    /// must not answer for bci `usize::MAX` or for anything else.
    #[test]
    fn a_siteless_slot_answers_for_nothing() {
        let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
        let slot = Box::new(JitMICSlot::new());
        slot.cached_class_id.store(77, Ordering::Relaxed);
        slot.cached_entry_word.store(ENTRY, Ordering::Relaxed);
        cm._jit_mic_slots.push(slot);
        assert_eq!(cm.dominant_receiver_at_bci(0), None);
        assert_eq!(cm.dominant_receiver_at_bci(16), None);
    }
}

/// the maps contain, and the failure mode if it is not — a real CHA
/// invalidation silently skipped, leaving a devirtualised call bound to a
/// method that now has a second implementor — is a miscompile, not a slowdown.
/// So each case below is the equivalence, not the speed.
#[cfg(test)]
mod local_handler_site_cache {
    use super::*;

    /// The whole reason the cache is ONE `u64` and not two `u32`s: a reader
    /// must never be able to pair one throwable's class id with another
    /// throwable's answer. Round-tripping every field together is what says
    /// the packing has that property.
    #[test]
    fn the_cache_round_trips_class_and_index_together() {
        for (class_id, index) in [(1u32, 0i32), (0, 3), (0x7fff_ffff, -1), (1163, 2)] {
            let raw = JitLocalHandlerSite::encode_cache(class_id, index);
            assert_ne!(
                raw,
                JitLocalHandlerSite::CACHE_EMPTY,
                "a populated cache must be distinguishable from an empty one, \
                 including for class id 0 and index -1"
            );
            assert_eq!(
                JitLocalHandlerSite::decode_cache(raw),
                Some((class_id, index)),
                "class {class_id} / index {index} did not survive the packing"
            );
        }
    }

    /// `-1` — "this frame does not catch it" — is worth caching: a site that
    /// keeps propagating must not re-resolve its catch types on every throw.
    /// So the empty state cannot be "index is negative"; it is its own bit.
    #[test]
    fn an_empty_cache_is_not_a_cached_propagate() {
        assert_eq!(
            JitLocalHandlerSite::decode_cache(JitLocalHandlerSite::CACHE_EMPTY),
            None
        );
        let cached_propagate = JitLocalHandlerSite::encode_cache(7, -1);
        assert_eq!(
            JitLocalHandlerSite::decode_cache(cached_propagate),
            Some((7, -1))
        );
    }
}

#[cfg(test)]
mod invalidate_early_out {
    use super::*;

    fn body(inlined: &[(&str, &str, &str)]) -> CompiledMethod {
        let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
        cm.inlined_methods = inlined
            .iter()
            .map(|(c, m, d)| (c.to_string(), m.to_string(), d.to_string()))
            .collect();
        cm
    }

    const CID: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0);

    #[test]
    fn a_class_that_was_inlined_still_evicts() {
        let cache = JitCache::new();
        cache.put("Caller".into(), "a".into(), "()V".into(), CID, body(&[]));
        cache.put(
            "Caller".into(),
            "b".into(),
            "()V".into(),
            CID,
            body(&[("Helper", "getX", "()I")]),
        );
        assert_eq!(
            cache.invalidate_for_class("Helper"),
            1,
            "the early-out swallowed a real CHA invalidation"
        );
    }

    #[test]
    fn a_class_nobody_inlined_returns_zero_without_scanning() {
        let cache = JitCache::new();
        cache.put(
            "Caller".into(),
            "b".into(),
            "()V".into(),
            CID,
            body(&[("Alpha", "foo", "()V")]),
        );
        assert_eq!(cache.invalidate_for_class("Beta"), 0);
        assert_eq!(cache.len(), 1, "a non-matching invalidation evicted a body");
    }

    /// The set is only ever added to, so a body that is evicted leaves its
    /// class name behind. That must degrade to "scan and find nothing", not to
    /// a wrong answer.
    #[test]
    fn a_stale_name_costs_a_scan_and_still_answers_zero() {
        let cache = JitCache::new();
        cache.put(
            "Caller".into(),
            "b".into(),
            "()V".into(),
            CID,
            body(&[("Helper", "getX", "()I")]),
        );
        assert_eq!(cache.invalidate_for_class("Helper"), 1);
        // "Helper" is still in the name set; the cache no longer holds it.
        assert_eq!(cache.invalidate_for_class("Helper"), 0);
    }

    /// `invalidate_unloaded_class` ALSO matches on `declaring_class_id`, which
    /// the name set says nothing about. It deliberately has no early-out, and a
    /// future edit that gives it one by symmetry would drop every body whose
    /// class was unloaded without ever having been inlined from.
    #[test]
    fn unloaded_class_eviction_does_not_depend_on_the_inlined_name_set() {
        let cache = JitCache::new();
        let owner = cratonvm_types::ClassId::new(7);
        cache.put("Doomed".into(), "m".into(), "()V".into(), owner, body(&[]));
        assert_eq!(
            cache.invalidate_unloaded_class(owner, "Doomed"),
            1,
            "an unloaded class's own body survived: `invalidate_unloaded_class` \
             matches by declaring_class_id and must not be gated on the \
             inlined-name set"
        );
    }

    #[test]
    fn clear_all_empties_the_name_set_with_the_maps() {
        let cache = JitCache::new();
        cache.put(
            "Caller".into(),
            "b".into(),
            "()V".into(),
            CID,
            body(&[("Helper", "getX", "()I")]),
        );
        cache.clear_all();
        assert!(!cache.any_body_inlined_from("Helper"));
        assert_eq!(cache.invalidate_for_class("Helper"), 0);
    }
}

/// Census of `execute()`'s static JIT-eligibility gate: how often the positive
/// memo (`JitRealm::jit_gate_pass`) answered, versus how often the full gate —
/// including `jit_method_calls_native_shadowed`'s O(method-bytecode) decode —
/// had to run and fill it.
///
/// `fills` is bounded by the number of distinct eligible methods; `hits` is the
/// number of `execute()` entries that would previously have re-run the whole
/// gate. The ratio is the fix's whole justification, so it is measured rather
/// than asserted. Always on.
///
/// STRIPED, because "always on" is only affordable that way. The hit side runs
/// on every interpreted `execute()` entry of an already-eligible method, from
/// every mutator thread at once, and a `fetch_add` on ONE process-wide word
/// there is a cache line bounced between every core that runs Java — the same
/// shape `validate_code_ptr`'s census and `ACTIVE_JIT_EXECUTIONS` were each
/// rewritten to avoid. The shared `RwLock` read next to it does not make that
/// free: a read lock is its own contended line, and adding a second one
/// doubles it. A [`StripedCounter`] puts each thread on its own line and pays
/// the sum only when the stats dump reads it.
///
/// [`StripedCounter`]: cratonvm_types::striped_counter::StripedCounter
static JIT_GATE_PASS_HITS: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();
/// Bounded by the number of distinct eligible methods, so it is not hot; it is
/// striped too only so the pair is read the same way.
static JIT_GATE_PASS_FILLS: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();

#[inline]
pub fn note_jit_gate_pass_hit() {
    JIT_GATE_PASS_HITS.inc();
}

#[inline]
pub fn note_jit_gate_pass_fill() {
    JIT_GATE_PASS_FILLS.inc();
}

/// `(hits, fills)` for the stats dump. Each is a sum over the stripes, so a
/// reading taken while mutators run can be off by the increments in flight —
/// the same tolerance the relaxed counters it replaced had.
pub fn jit_gate_pass_census() -> (u64, u64) {
    (
        JIT_GATE_PASS_HITS.get() as u64,
        JIT_GATE_PASS_FILLS.get() as u64,
    )
}

/// See [`ir_lower::ic_frame_republish_sites`].
pub use crate::ir_lower::ic_frame_republish_sites;

#[cfg(test)]
mod devirt_intrinsic_yield_tests {
    use super::*;

    /// `java/lang/String` is `final`, so `invokevirtual_site_final_owner`
    /// answers for every one of its call sites and `try_compile_inner` used to
    /// rewrite `invoke_kind` 0 -> 1 on that answer — putting the site into the
    /// inline/direct-bind ladder, which `continue`s, past an instance-intrinsic
    /// gate that is `invoke_kind == 0 || invoke_kind == 2`.
    ///
    /// The result was a real `CALL` into `String.charAt` on every character,
    /// and it was invisible: the three `string-intrinsic` diagnostics all sit
    /// past the point the site left. Measured on `probes/CharAtDoorProbe.java`
    /// at 349.64 ns/char against 3.2-4.3 for four byte-identical siblings the
    /// OSR door compiled.
    #[test]
    fn the_three_string_accessors_hold_the_site_back_from_a_static_bind() {
        let layout = Some(STRING_INTRINSIC_NAME_PROBE);
        for (name, desc) in [("charAt", "(I)C"), ("length", "()I"), ("isEmpty", "()Z")] {
            assert!(
                site_yields_to_call_site_intrinsic("java/lang/String", name, desc, layout),
                "String.{name}{desc} must keep its call-site intrinsic"
            );
        }
    }

    /// The JVMS 5.4.6 rule the rewrite exists for is untouched, and this is why:
    /// no intrinsic matches a private method, so a private target never yields
    /// and stays pinned to its declaring class. `String.isLatin1()Z` is the
    /// concrete one — private, and reached constantly from the very chain the
    /// missing intrinsic sends the program down.
    #[test]
    fn a_private_target_never_yields_so_the_dispatch_rule_is_intact() {
        let layout = Some(STRING_INTRINSIC_NAME_PROBE);
        for (name, desc) in [
            ("isLatin1", "()Z"),
            ("coder", "()B"),
            ("checkIndex", "(II)V"),
        ] {
            assert!(
                !site_yields_to_call_site_intrinsic("java/lang/String", name, desc, layout),
                "String.{name}{desc} is not an intrinsic and must stay statically bound"
            );
        }
    }

    /// Without a resolved `StringFieldLayout` the intrinsic cannot be emitted
    /// either, so there is nothing to hold the site back FOR — it keeps the
    /// static bind it had. Handing `None` is exactly what a door with no
    /// layout resolver does.
    #[test]
    fn no_layout_means_no_yield() {
        assert!(
            !site_yields_to_call_site_intrinsic("java/lang/String", "charAt", "(I)C", None),
            "with no layout the intrinsic is unemittable; do not cost the site its bind"
        );
    }

    /// An ordinary method on a non-intrinsic class is unaffected in both
    /// directions — this predicate must not become a blanket "never
    /// devirtualise".
    #[test]
    fn an_ordinary_site_is_untouched() {
        let layout = Some(STRING_INTRINSIC_NAME_PROBE);
        assert!(!site_yields_to_call_site_intrinsic(
            "com/example/Widget",
            "charAt",
            "(I)C",
            layout
        ));
        assert!(!site_yields_to_call_site_intrinsic(
            "java/lang/String",
            "trim",
            "()Ljava/lang/String;",
            layout
        ));
    }

    /// `CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD=1` is the B arm, and it has to
    /// restore the old ordering exactly — otherwise the A/B compares two
    /// things. Read through the flag machinery's thread override so the test
    /// does not depend on the developer's ambient environment.
    #[test]
    fn the_kill_switch_restores_the_static_bind() {
        let layout = Some(STRING_INTRINSIC_NAME_PROBE);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD", Some("1"))],
            || {
                assert!(
                    !site_yields_to_call_site_intrinsic(
                        "java/lang/String",
                        "charAt",
                        "(I)C",
                        layout
                    ),
                    "the opt-out must hand the site back to the static bind"
                );
            },
        );
    }
}

#[cfg(test)]
mod deferred_new_retry_gate_tests {
    // `pub(crate)` in `jfr_compile_decision`, so not reachable through the
    // crate root's glob re-export; see the file preamble.
    use crate::jfr_compile_decision::{note_deferred_new_bail, take_deferred_new_retry};

    /// The memo map and its held-count are process-global, so these tests
    /// cannot run concurrently with each other: `cargo test` runs them on
    /// separate threads, and a count asserted as `before + 1` is only stable
    /// if nothing else is arming or spending at the same time.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The retry exists for a TRANSIENT condition, so it must not be spent
    /// while that condition still holds. Before this gate the grant was blind:
    /// it flipped the one-shot memo on the next attempt regardless, the attempt
    /// bailed exactly as the first had, and the method then had no retry left
    /// for the moment its class actually loaded. Measured on CratonBench, that
    /// lost 5 of 6 retries and cost 30 ms of background compile per run.
    #[test]
    fn an_unresolved_site_holds_the_retry_instead_of_spending_it() {
        let _serial = serial();
        let (c, m, d) = ("T$Hold", "run", "()V");
        note_deferred_new_bail(c, m, d, &[(7, 11)]);
        let never = |_: u32, _: u16| false;
        assert!(
            !take_deferred_new_retry(c, m, d, &never),
            "a still-unresolved new site must not consume the one retry",
        );
        // And the memo survives: the whole point is that it is still there when
        // the class does load.
        let now = |_: u32, _: u16| true;
        assert!(
            take_deferred_new_retry(c, m, d, &now),
            "the retry held above must still be available once the site resolves",
        );
    }

    /// One-shot is one-shot: a granted retry is not re-grantable.
    #[test]
    fn a_spent_retry_is_not_granted_twice() {
        let _serial = serial();
        let (c, m, d) = ("T$Once", "run", "()V");
        note_deferred_new_bail(c, m, d, &[(1, 2)]);
        let now = |_: u32, _: u16| true;
        assert!(take_deferred_new_retry(c, m, d, &now));
        assert!(!take_deferred_new_retry(c, m, d, &now));
    }

    /// Every site must resolve, not merely one of them: a method bails on the
    /// FIRST `new` the builder cannot type, so a retry granted while any site
    /// is still unresolved is a retry spent on the same bail.
    #[test]
    fn one_unresolved_site_among_several_still_holds() {
        let _serial = serial();
        let (c, m, d) = ("T$Partial", "run", "()V");
        note_deferred_new_bail(c, m, d, &[(1, 2), (1, 3)]);
        let only_first = |_h: u32, cp: u16| cp == 2;
        assert!(!take_deferred_new_retry(c, m, d, &only_first));
    }

    /// A memo with no recorded sites cannot answer the question, so it grants —
    /// the historical behaviour, not a silent refusal.
    #[test]
    fn a_memo_without_sites_grants_as_before() {
        let _serial = serial();
        let (c, m, d) = ("T$Unknown", "run", "()V");
        note_deferred_new_bail(c, m, d, &[]);
        let never = |_: u32, _: u16| false;
        assert!(take_deferred_new_retry(c, m, d, &never));
    }

    /// An un-armed method has no retry to take.
    #[test]
    fn a_method_that_never_bailed_has_no_retry() {
        let _serial = serial();
        let now = |_: u32, _: u16| true;
        assert!(!take_deferred_new_retry("T$Never", "run", "()V", &now));
    }

    /// A held retry has to be FINDABLE, or holding it is the same outcome as
    /// spending it: the method already has a body, so nothing compiles it
    /// again and no door it walks through will ever ask about it. The memo map
    /// is keyed by a hash, which cannot name a method — hence the stored key.
    #[test]
    fn a_held_retry_can_be_found_again_by_method_name() {
        let _serial = serial();
        let (c, m, d) = ("T$Findable", "run", "()V");
        super::note_deferred_new_bail(c, m, d, &[(3, 4)]);
        let never = |_: u32, _: u16| false;
        assert!(!take_deferred_new_retry(c, m, d, &never));
        let held = super::held_deferred_new_methods();
        assert!(
            held.iter()
                .any(|(hc, hm, hd)| &**hc == c && &**hm == m && &**hd == d),
            "a held retry must appear in the set the class-definition sweep reads",
        );
    }

    /// The sweep's fast path is this counter, so it has to actually track the
    /// arm/spend pair — a counter stuck at zero silently disables the sweep,
    /// and one that never decrements makes every class definition do work.
    #[test]
    fn the_held_count_tracks_arming_and_spending() {
        let _serial = serial();
        let (c, m, d) = ("T$Counted", "run", "()V");
        let before = super::held_deferred_new_count();
        super::note_deferred_new_bail(c, m, d, &[(5, 6)]);
        assert_eq!(super::held_deferred_new_count(), before + 1);
        let now = |_: u32, _: u16| true;
        assert!(take_deferred_new_retry(c, m, d, &now));
        assert_eq!(super::held_deferred_new_count(), before);
    }

    /// A class that never loads must stop costing a resolution per class
    /// definition. Before the budget, the sweep re-resolved one such memo 3,905
    /// times in a single H2 run, each time under the class-manager read lock.
    ///
    /// The assertion is on the HELD COUNT rather than on the return value,
    /// because both a held and a retired memo return `false` — what separates
    /// them is that a retired one stops the sweep's fast path from firing at
    /// all, and that is the property worth pinning.
    #[test]
    fn a_class_that_never_loads_retires_its_memo_within_the_look_budget() {
        let _serial = serial();
        let (c, m, d) = ("T$Forever", "run", "()V");
        let before = super::held_deferred_new_count();
        super::note_deferred_new_bail(c, m, d, &[(9, 9)]);
        assert_eq!(super::held_deferred_new_count(), before + 1);
        let never = |_: u32, _: u16| false;
        for _ in 0..super::MAX_DEFERRED_NEW_LOOKS {
            assert!(!take_deferred_new_retry(c, m, d, &never));
        }
        assert_eq!(
            super::held_deferred_new_count(),
            before,
            "a memo asked {} times and refused every time must be retired, or the \
             class-definition sweep keeps paying for it forever",
            super::MAX_DEFERRED_NEW_LOOKS,
        );
        // And it stays retired: `note_deferred_new_bail`'s `or_insert` must not
        // re-arm it, or the compile door restarts the budget.
        super::note_deferred_new_bail(c, m, d, &[(9, 9)]);
        assert_eq!(super::held_deferred_new_count(), before);
        // A retired memo is also invisible to the sweep's work list.
        assert!(
            !super::held_deferred_new_methods()
                .iter()
                .any(|(hc, hm, hd)| &**hc == c && &**hm == m && &**hd == d),
            "a retired memo must not appear in the sweep's work list",
        );
    }

    /// The budget must not touch a class that loads promptly, which is every
    /// case the mechanism exists for. One look short of the budget still holds,
    /// and the grant still lands.
    #[test]
    fn a_memo_within_budget_still_gets_its_retry() {
        let _serial = serial();
        let (c, m, d) = ("T$Late", "run", "()V");
        super::note_deferred_new_bail(c, m, d, &[(4, 4)]);
        let never = |_: u32, _: u16| false;
        for _ in 0..(super::MAX_DEFERRED_NEW_LOOKS - 1) {
            assert!(!take_deferred_new_retry(c, m, d, &never));
        }
        let now = |_: u32, _: u16| true;
        assert!(
            take_deferred_new_retry(c, m, d, &now),
            "a memo one look inside the budget must still be grantable",
        );
    }

    /// `CRATONVM_JIT_DEFERRED_NEW_LOOKS=0` is the kill switch, and a kill
    /// switch nobody exercises is not a kill switch. Unbounded means the memo
    /// survives well past the default budget.
    #[test]
    fn the_zero_budget_restores_the_unbounded_behaviour() {
        let _serial = serial();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_DEFERRED_NEW_LOOKS", Some("0"))],
            || {
                // No early return, and that is the point: the budget is read
                // UNCACHED under `cfg(test)`, so the thread-scoped override
                // always takes effect on this thread and never on any other.
                // This used to bail whenever another test had already
                // initialised a process-wide `OnceLock`, which on a parallel
                // run was most of the time -- so the kill switch this test
                // names went unexercised. See `deferred_new_look_budget`.
                assert_eq!(
                    super::deferred_new_look_budget(),
                    0,
                    "the override must reach the budget, or everything below \
                     asserts the DEFAULT behaviour under a kill-switch name",
                );
                let (c, m, d) = ("T$Unbounded", "run", "()V");
                let before = super::held_deferred_new_count();
                super::note_deferred_new_bail(c, m, d, &[(2, 2)]);
                let never = |_: u32, _: u16| false;
                for _ in 0..(super::MAX_DEFERRED_NEW_LOOKS * 4) {
                    assert!(!take_deferred_new_retry(c, m, d, &never));
                }
                assert_eq!(
                    super::held_deferred_new_count(),
                    before + 1,
                    "an unbounded budget must never retire a memo",
                );
            },
        );
    }
}

#[cfg(test)]
mod deopt_saved_register_range_tests {
    use crate::FrameLayout;

    /// The geometry both backends publish, as the emitters write it:
    /// `gpr[r] -> [rbp - (base - r*8)]` for `r` in `0..16` and
    /// `xmm[n] -> [rbp - (base - 128 - n*8)]` for `n` in `0..16`, over one
    /// 256-byte region whose DEEPEST offset is `base`. Since round 9 wave 7b
    /// (wire7b) the single-pass producer places that region directly after the
    /// blind spill (`xmm[15]` at `base - 248 == reg_spill_hi`) and the outgoing
    /// reserve starts one slot past `gpr[0]` (`outgoing_lo == base + 8`); this
    /// fixture mirrors that.
    fn layout_with_deopt_regs(base: i32) -> FrameLayout {
        FrameLayout {
            reg_spill_lo: base - 248 - 112,
            reg_spill_hi: base - 248,
            outgoing_lo: base + 8,
            deopt_gpr_lo: base - 15 * 8,
            deopt_gpr_hi: base + 8,
            deopt_xmm_lo: base - 248,
            deopt_xmm_hi: base - 120,
            frame_size: base + 64,
            ..Default::default()
        }
    }

    /// Every slot the deopt stub writes is classified, and the two halves do
    /// not overlap. Written as a sweep rather than as endpoint assertions
    /// because the failure this guards against is an off-by-one-slot at the
    /// seam, where `gpr[15]` and `xmm[0]` meet.
    #[test]
    fn the_two_halves_partition_the_saved_registers_region() {
        let base = 848;
        let l = layout_with_deopt_regs(base);
        for r in 0..16i32 {
            let off = base - r * 8;
            assert!(
                l.is_deopt_saved_gpr_image(off),
                "gpr[{r}] at off={off} must be in the GPR half",
            );
            assert!(
                !l.is_deopt_saved_xmm_image(off),
                "gpr[{r}] at off={off} must not also be in the XMM half",
            );
            assert_eq!(l.region_name(off), "deopt-saved-gpr-image");
        }
        for n in 0..16i32 {
            let off = base - 128 - n * 8;
            assert!(
                l.is_deopt_saved_xmm_image(off),
                "xmm[{n}] at off={off} must be in the XMM half",
            );
            assert!(
                !l.is_deopt_saved_gpr_image(off),
                "xmm[{n}] at off={off} must not also be in the GPR half",
            );
            assert_eq!(l.region_name(off), "deopt-saved-xmm-image");
        }
    }

    /// The slot immediately outside each end is NOT claimed. The region is a
    /// licence to treat words differently -- the GPR half gets a WRITE and the
    /// XMM half gets its pin dropped -- so a range that runs one slot long
    /// reaches storage neither argument covers.
    #[test]
    fn neither_half_claims_a_slot_outside_the_region() {
        let base = 848;
        let l = layout_with_deopt_regs(base);
        assert!(
            !l.is_deopt_saved_gpr_image(base + 8),
            "one slot deeper than gpr[0]"
        );
        assert!(!l.is_deopt_saved_xmm_image(base + 8));
        assert!(
            !l.is_deopt_saved_xmm_image(base - 256),
            "one slot past xmm[15]"
        );
        assert!(!l.is_deopt_saved_gpr_image(base - 256));
        // Deliberately NOT asserted: which name `region_name` gives the slot
        // one past `xmm[15]` (`base - 256`). With the wave-7b placement that is
        // the blind spill's deepest word; pinning it here would be asserting
        // the arithmetic of an unrelated reservation.
    }

    /// A frame that reserved no `SavedRegisters` region publishes `(0, 0)`, and
    /// both predicates must read that as "absent" rather than as a range
    /// containing 0 -- offset 0 is `[rbp]`, the saved caller frame pointer.
    #[test]
    fn an_unreserved_region_claims_nothing() {
        let l = FrameLayout {
            reg_spill_lo: 64,
            reg_spill_hi: 176,
            outgoing_lo: 176,
            ..Default::default()
        };
        for off in [0, 8, 64, 176, 400, 848] {
            assert!(!l.is_deopt_saved_gpr_image(off), "off={off}");
            assert!(!l.is_deopt_saved_xmm_image(off), "off={off}");
        }
        assert_eq!(l.region_name(400), "outgoing-args-or-deopt-regs");
    }
}

/// Regressions from the 2026-09-12 JIT review: array scalar replacement may
/// forward a stored value to a narrow load only when the value already fits.
#[cfg(test)]
mod narrow_array_forwarding_tests {
    // `pub(crate)` in `ea_ir_bridge`; see the file preamble for why the
    // crate root's glob cannot carry it.
    use crate::ea_ir_bridge::narrow_array_value_fits;

    use crate::ir::{Graph, IrType, MemKind, NodeId, Op};

    fn graph() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        }
    }

    fn konst(g: &mut Graph, v: i64) -> NodeId {
        g.add(Op::Const(v), IrType::Int, vec![], None)
    }

    #[test]
    fn a_constant_must_be_in_the_slot_range() {
        let mut g = graph();
        let c300 = konst(&mut g, 300);
        let c44 = konst(&mut g, 44);
        let c_neg = konst(&mut g, -1);
        assert!(
            !narrow_array_value_fits(&g, c300, MemKind::Byte, Some(8)),
            "300 does not fit a byte"
        );
        assert!(narrow_array_value_fits(&g, c44, MemKind::Byte, Some(8)));
        assert!(
            !narrow_array_value_fits(&g, c_neg, MemKind::Char, Some(5)),
            "char is unsigned"
        );
        assert!(narrow_array_value_fits(&g, c300, MemKind::Short, Some(9)));
        // Wide element kinds store the whole value.
        assert!(narrow_array_value_fits(&g, c300, MemKind::Int, Some(10)));
    }

    #[test]
    fn the_builders_own_narrowing_shapes_fit() {
        let mut g = graph();
        let x = g.add(Op::Param(0), IrType::Int, vec![], None);
        let k24 = konst(&mut g, 24);
        let shl = g.add(Op::Shl, IrType::Int, vec![x, k24], None);
        let i2b = g.add(Op::Shr, IrType::Int, vec![shl, k24], None);
        assert!(narrow_array_value_fits(&g, i2b, MemKind::Byte, Some(8)));
        assert!(!narrow_array_value_fits(&g, i2b, MemKind::Char, Some(5)));
        let mask = konst(&mut g, 0xFFFF);
        let i2c = g.add(Op::And, IrType::Int, vec![x, mask], None);
        assert!(narrow_array_value_fits(&g, i2c, MemKind::Char, Some(5)));
        // An unnarrowed parameter fits no narrow slot.
        assert!(!narrow_array_value_fits(&g, x, MemKind::Short, Some(9)));
    }

    #[test]
    fn a_boolean_slot_takes_only_zero_or_one() {
        let mut g = graph();
        let one = konst(&mut g, 1);
        let two = konst(&mut g, 2);
        assert!(narrow_array_value_fits(&g, one, MemKind::Byte, Some(4)));
        assert!(
            !narrow_array_value_fits(&g, two, MemKind::Byte, Some(4)),
            "boolean[] stores value & 1"
        );
    }
}
