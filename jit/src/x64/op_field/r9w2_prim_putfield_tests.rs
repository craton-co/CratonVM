use super::*;
use cratonvm_types::Value;

/// The two words `write_value_atomic` — the helper's legacy-cell writer —
/// leaves in a 16-byte cell for `v`.
fn cell_of(v: Value) -> [u64; 2] {
    let mut cell = [0xAAAA_AAAA_AAAA_AAAAu64; 2];
    // SAFETY: a 16-byte, 8-aligned buffer is exactly one `Value` cell.
    unsafe { cratonvm_types::write_value_atomic(cell.as_mut_ptr() as *mut Value, v) };
    cell
}

/// The inline legacy store writes word 0 = `tag | payload32 << 32`, word
/// 1 = 0 for a narrow value and word 0 = `tag`, word 1 = payload for a wide
/// one. Those are the helper's words exactly, or a compiled store and an
/// interpreted one leave different cells behind.
#[test]
fn legacy_cell_words_match_the_helper_s() {
    let w = cell_of(Value::Int(-5));
    assert_eq!(w[0] & 0xFFFF_FFFF, u64::from(legacy_cell_tag_for(b'I')));
    assert_eq!(w[0] >> 32, 0xFFFF_FFFB, "the Int payload is the low dword");
    assert_eq!(w[1], 0, "word 1 of a narrow value is zero");

    let w = cell_of(Value::Float(1.5));
    assert_eq!(
        w[0],
        u64::from(legacy_cell_tag_for(b'F')) | (u64::from(1.5f32.to_bits()) << 32)
    );
    assert_eq!(w[1], 0);

    let w = cell_of(Value::Long(-7));
    assert_eq!(
        w,
        [u64::from(legacy_cell_tag_for(b'J')), 0xFFFF_FFFF_FFFF_FFF9]
    );
    let w = cell_of(Value::Double(2.5));
    assert_eq!(w, [u64::from(legacy_cell_tag_for(b'D')), 2.5f64.to_bits()]);

    for t in *b"ZBCS" {
        assert_eq!(
            legacy_cell_tag_for(t),
            legacy_cell_tag_for(b'I'),
            "a sub-int field's cell is a `Value::Int`"
        );
    }
}

/// The compact store writes the field's OWN width
/// (`write_compact_field`): one byte for `Z`/`B`, two for `C`/`S`, four
/// for `I`/`F`, eight for `J`/`D` — never a wider store that would clobber
/// the neighbouring packed field.
#[test]
fn compact_stores_have_the_field_s_own_width() {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let cases: [(u8, &[u8]); 8] = [
        (b'Z', &[0x88, 0x90]),
        (b'B', &[0x88, 0x90]),
        (b'C', &[0x66, 0x89, 0x90]),
        (b'S', &[0x66, 0x89, 0x90]),
        (b'I', &[0x89, 0x90]),
        (b'F', &[0x89, 0x90]),
        (b'J', &[0x48, 0x89, 0x90]),
        (b'D', &[0x48, 0x89, 0x90]),
    ];
    for (tag, opcode) in cases {
        let start = c.buf.pos();
        c.emit_store_rdx_width(0x1234, tag);
        let got: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
        let mut want: Vec<u8> = opcode.to_vec();
        want.extend_from_slice(&0x1234i32.to_le_bytes());
        assert_eq!(got, want, "compact store of a `{}` field", tag as char);        }    }