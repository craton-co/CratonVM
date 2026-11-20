use super::*;

/// JIT round 21 wave 1, lane updaters: a tagged table's screen is the
/// byte-offset decode followed by exactly the untagged screen (whose
/// immediates carry the UNTAGGED table address), and its decline edges
/// are the decode's `JNZ` plus the untagged edges shifted past it. Both
/// halves (int/long and reference).
#[test]
fn r21_updaters_tagged_table_screen_is_the_decode_then_the_plain_screen() {
    let (decode, edge) = unsafe_byte_field_offset_decode_bytes();
    assert_eq!(
        decode,
        vec![
            0x49, 0x83, 0xEB, 0x10, // SUB R11, 16
            0x41, 0xF6, 0xC3, 0x07, // TEST R11B, 7
            0x0F, 0x85, 0, 0, 0, 0, // JNZ rel32
            0x49, 0xC1, 0xEB, 0x03, // SHR R11, 3
        ]
    );
    assert_eq!(edge, 10);
    let tagged = 0x10_0000 | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG;
    let pairs = [
        (
            unsafe_inline_field_cell_bytes(0x10_0000, 0x20_0000, 4),
            unsafe_inline_field_cell_bytes(tagged, 0x20_0000, 4),
        ),
        (
            unsafe_inline_field_cell_bytes(0x10_0000, 0x20_0000, 8),
            unsafe_inline_field_cell_bytes(tagged, 0x20_0000, 8),
        ),
        (
            unsafe_inline_ref_field_cell_bytes(0x10_0000, 0x20_0000),
            unsafe_inline_ref_field_cell_bytes(tagged, 0x20_0000),
        ),
    ];
    for ((plain, plain_edges), (with_decode, decode_edges)) in pairs {
        assert_eq!(&with_decode[..decode.len()], &decode[..]);
        assert_eq!(&with_decode[decode.len()..], &plain[..]);
        assert_eq!(decode_edges.len(), plain_edges.len() + 1);
        assert_eq!(decode_edges[0], edge);
        for (t, p) in decode_edges[1..].iter().zip(&plain_edges) {
            assert_eq!(*t, p + decode.len());
        }
    }
}

/// The decode's arithmetic against the natives' encode
/// (`16 + slot * 8`): every cell-keyable slot round-trips, and an offset
/// below the base, inside a field, past the key, in the synthetic band or
/// the `Buffer.address` sentinel never names a slot.
#[test]
fn r21_updaters_byte_offset_decode_model_matches_the_encode() {
    let decode = |off: u64| -> Option<u64> {
        let rel = off.wrapping_sub(16);
        if rel & 7 != 0 {
            return None;
        }
        let slot = rel >> 3;
        (slot <= UNSAFE_FIELD_CELL_MAX_SLOT as u64).then_some(slot)
    };
    for slot in [0u64, 1, 2, 7, 0x7FFE, 0x7FFF] {
        assert_eq!(decode(16 + slot * 8), Some(slot));
    }
    for bad in [
        0u64,
        8,
        15,
        17,
        20,
        16 + 0x8000 * 8,
        0x2000_0000,
        0x3000_0000,
        0x7fff_ffff_ffff_fffe,
        u64::MAX - 7,
    ] {
        assert_eq!(decode(bad), None, "offset {bad:#x}");
    }
}

/// JIT round 21 wave 24 (lane flipprep, flip review #14 / #15): an
/// Unsafe ARRAY offset is a real byte offset (`16 + 8*i` for `long[]` /
/// `Object[]`), so the decode above maps it to a slot `i`, and an array's
/// class word is its COMPONENT class id, so the cell key alone would alias
/// that class's field cell (CHM `tabAt` reading a `Node` field cell). The
/// only thing between them is the screen's `TEST BYTE [RAX + GC_FLAGS],
/// GC_FLAG_COMPACT ; JZ`, so pin that no array header ever carries the
/// bit: the constructor both inline array allocators take their mark word
/// from (`ObjectHeader::new`, SP `objects.rs` and IR `runtime_lowering.rs`),
/// the collector's own `alloc_array`, and the mark / old-gen transitions a
/// live array goes through. A future "compact array header" fails here
/// instead of turning the byte-offset field cells into an array alias.
#[test]
fn r21_flipprep_no_array_header_carries_gc_flag_compact() {
    use cratonvm_types::{
        ArrayElementType, ClassId, ObjectHeader, ObjectKind, GC_FLAGS_BYTE_OFFSET,
        GC_FLAG_COMPACT, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, MARK_WORD_OFFSET,
    };
    // The byte the screen's TEST reads: the mark word's byte at
    // `GC_FLAGS_BYTE_OFFSET` (little-endian, the mark word at
    // `MARK_WORD_OFFSET`).
    let screen_byte = |mark: u32| (mark >> (8 * (GC_FLAGS_BYTE_OFFSET - MARK_WORD_OFFSET))) as u8;
    let all = [
        ArrayElementType::Boolean,
        ArrayElementType::Char,
        ArrayElementType::Float,
        ArrayElementType::Double,
        ArrayElementType::Byte,
        ArrayElementType::Short,
        ArrayElementType::Int,
        ArrayElementType::Long,
        ArrayElementType::Reference,
    ];
    for elem in all {
        for (class_word, len) in [(0u32, 0u32), (0x0F13, 1), (0x0F13, 17), (7, 0x7FFF)] {
            let header = ObjectHeader::new(ClassId::new(class_word), ObjectKind::Array, elem, len, len);
            let mark = header.mark_word.load(std::sync::atomic::Ordering::Relaxed);
            assert_eq!(screen_byte(mark) & GC_FLAG_COMPACT, 0, "{elem:?}[{len}]: the inline arms' mark word");
            assert_eq!(header.gc_flags() & GC_FLAG_COMPACT, 0);
            assert!(!ObjectHeader::is_short_mark(mark));
            header.add_gc_flags(GC_FLAG_MARKED | GC_FLAG_OLD_GEN);
            header.clear_gc_flags(GC_FLAG_MARKED);
            let after = header.mark_word.load(std::sync::atomic::Ordering::Relaxed);
            assert_eq!(screen_byte(after) & GC_FLAG_COMPACT, 0, "{elem:?}: after mark / promote");
        }
    }
    let heap = cratonvm_gc::Heap::new();
    for elem in all {
        for len in [0usize, 1, 9, 64] {
            let arr = heap.alloc_array(ClassId::new(0x0F13), elem, len);
            let header = heap.get_header(arr);
            assert_eq!(header.kind(), ObjectKind::Array);
            assert_eq!(header.gc_flags() & GC_FLAG_COMPACT, 0, "{elem:?}[{len}]: alloc_array");
            assert_eq!(
                screen_byte(header.mark_word.load(std::sync::atomic::Ordering::Relaxed)) & GC_FLAG_COMPACT,
                0
            );            }        }    }