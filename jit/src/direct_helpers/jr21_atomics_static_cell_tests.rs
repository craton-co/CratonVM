use super::*;

/// The tags the static screen compares are the `Value` discriminants a
/// static cell carries (`vm_exec::hw_static_cell_update`'s `0` / `1`),
/// read where the screen reads them.
#[test]
fn static_tags_are_the_value_discriminants() {
    let tag_of = |v: cratonvm_types::Value| -> u32 {
        // SAFETY: a `Value` is 16 bytes whose first word is the
        // discriminant (`FIELD_CELL_TAG_OFFSET == 0`).
        unsafe { *(&v as *const cratonvm_types::Value as *const u32) }
    };
    assert_eq!(
        tag_of(cratonvm_types::Value::Int(-7)),
        u32::from(UNSAFE_STATIC_TAG_INT)
    );
    assert_eq!(
        tag_of(cratonvm_types::Value::Long(-7)),
        u32::from(UNSAFE_STATIC_TAG_LONG)
    );
}

/// Keys are never the empty word for a band offset, and the index is the
/// screen's `(off ^ off >> 3) & (COUNT - 1)`.
#[test]
fn static_keys_and_indices() {
    for off in [
        UNSAFE_STATIC_OFFSET_FIRST,
        UNSAFE_STATIC_OFFSET_FIRST + 1,
        0x3000_0000,
        0x3000_0008,
        UNSAFE_STATIC_OFFSET_LAST,
    ] {
        assert_ne!(unsafe_static_cell_key(off, false), 0);
        assert_eq!(
            unsafe_static_cell_key(off, true),
            unsafe_static_cell_key(off, false) | 1
        );
        let i = unsafe_static_cell_index(off);
        assert!(i < UNSAFE_STATIC_CELL_COUNT);
        // Casts: a band offset fits 32 bits, as the screen computes it.
        assert_eq!(i, ((off as u32 ^ (off as u32 >> 3)) & 127) as usize);
    }
    // Consecutive minted offsets and consecutive scaled ones spread.
    assert_ne!(
        unsafe_static_cell_index(0x2000_0000),
        unsafe_static_cell_index(0x2000_0001)
    );
    assert_ne!(
        unsafe_static_cell_index(0x3000_0000),
        unsafe_static_cell_index(0x3000_0008)
    );
    // Casts: small positive constants. The halves follow each other:
    // int/long, reference, sub-word cells, then the static entries.
    assert_eq!(
        UNSAFE_SUBWORD_FIELD_CELL_BASE as usize,
        UNSAFE_REF_FIELD_CELL_BASE as usize + 8 * UNSAFE_FIELD_CELL_COUNT
    );
    assert_eq!(
        UNSAFE_STATIC_CELL_BASE,
        UNSAFE_SUBWORD_FIELD_CELL_BASE as usize + 8 * UNSAFE_FIELD_CELL_COUNT
    );
}

/// The screen's shape: every decline is a zeroed rel32 `Jcc`, the band
/// edge is the `JA`, the width picks the key bit, the tag and the payload.
#[test]
fn static_screen_bytes_shape() {
    let table = 0x0000_1122_3344_5000usize;
    let (b4, e4, ns4) = unsafe_inline_static_cell_bytes(table, 4);
    let (b8, e8, ns8) = unsafe_inline_static_cell_bytes(table, 8);
    assert_eq!(e4.len(), 4);
    assert_eq!(e8.len(), 4);
    assert_eq!(
        b8.len(),
        b4.len() + 4,
        "only the long key's OR RDX, 1 differs"
    );
    for (b, e, ns) in [(&b4, &e4, ns4), (&b8, &e8, ns8)] {
        for &at in e.iter().chain(std::iter::once(&ns)) {
            assert_eq!(b[at - 2], 0x0F, "a rel32 Jcc at {at}");
            assert_eq!(&b[at..at + 4], &[0, 0, 0, 0]);
        }
        assert_eq!(b[ns - 1], 0x87, "the band edge is JA");
        let has = |n: &[u8]| b.windows(n.len()).any(|w| w == n);
        let mut mov = vec![0x49, 0xBA];
        // Cast: an address.
        mov.extend_from_slice(&((table + UNSAFE_STATIC_CELL_BASE) as u64).to_le_bytes());
        assert!(has(&mov), "MOV R10, the static half");
        assert!(has(&[0x49, 0x3B, 0x14, 0x0A]), "CMP RDX, [R10+RCX]");
        assert!(
            has(&[0x49, 0x3B, 0x44, 0x0A, 0x08]),
            "CMP RAX, [R10+RCX+8]: the mirror proof"
        );
        assert!(
            has(&[0x45, 0x8B, 0x5C, 0x0A, 0x18]),
            "MOV R11D: the displacement half"
        );
    }
    let has = |b: &[u8], n: &[u8]| b.windows(n.len()).any(|w| w == n);
    assert!(has(&b4, &[0x42, 0x83, 0x3C, 0x18, UNSAFE_STATIC_TAG_INT]));
    assert!(has(&b8, &[0x42, 0x83, 0x3C, 0x18, UNSAFE_STATIC_TAG_LONG]));
    assert!(b4.ends_with(&[0x49, 0x83, 0xC3, 4]), "ADD R11, payload32");
    assert!(b8.ends_with(&[0x49, 0x83, 0xC3, 8]), "ADD R11, payload64");
    assert!(has(&b8, &[0x4B, 0x8D, 0x14, 0x1B, 0x48, 0x83, 0xCA, 0x01]));
}

/// No cell table, no static arm; the byte-offset tag bit is stripped.
#[test]
fn static_arm_table_follows_the_cell_table() {
    assert_eq!(unsafe_static_arm_table(&DirectHelperTable::EMPTY), None);
    if !cfg!(target_arch = "x86_64")
        || !unsafe_field_rmw_inline_enabled()
        || !unsafe_static_rmw_inline_enabled()
        || crate::x64::zgc_read_barrier_blocks_inline_fields()
    {
        return;
    }
    let t = DirectHelperTable {
        unsafe_field_cells: 0x8000 | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG,
        ..DirectHelperTable::EMPTY
    };
    assert_eq!(unsafe_static_arm_table(&t), Some(0x8000));    }

/// UNSAFE21-2: the four sub-word rows and their admission bits, above
/// every older row's; near misses are not sites.
#[test]
fn subword_sites_and_bits() {
    if !unsafe_subword_rmw_inline_enabled() {
        return;
    }
    let u = "jdk/internal/misc/Unsafe";
    let site = |m: &str, d: &str| unsafe_inline_subword_site(u, m, d);
    let cae_b = site("compareAndExchangeByte", "(Ljava/lang/Object;JBB)B").unwrap();
    let cae_s = site("compareAndExchangeShort", "(Ljava/lang/Object;JSS)S").unwrap();
    let cas_s = site("compareAndSetShort", "(Ljava/lang/Object;JSS)Z").unwrap();
    let cas_b = site("compareAndSetByte", "(Ljava/lang/Object;JBB)Z").unwrap();
    for (i, r) in [cae_b, cae_s, cas_s, cas_b].into_iter().enumerate() {
        assert_eq!(r.row(), i);
        let (m, d) = UnsafeSubwordRmw::GATE_BITS[i];
        assert_eq!(site(m, d), Some(r));
        assert!(r.admitted(1 << (UnsafeSubwordRmw::FIRST_BIT + i)));
        assert!(!r.admitted(!(1 << (UnsafeSubwordRmw::FIRST_BIT + i))));
    }
    for bit in (0..UNSAFE_QU_EXTRA_FIRST_BIT + UNSAFE_QU_EXTRA_GATE_BITS.len()).chain([
        UNSAFE_REF_CAS_GATE_BIT,
        UNSAFE_REF_VOLATILE_GET_GATE_BIT,
        UNSAFE_REF_VOLATILE_PUT_GATE_BIT,
        UNSAFE_FIELD_ARM_BAKED_BIT,
        UNSAFE_REF_GET_AND_SET_GATE_BIT,
    ]) {
        assert!(bit < UnsafeSubwordRmw::FIRST_BIT, "bit {bit}");
    }
    assert!(site("compareAndExchangeChar", "(Ljava/lang/Object;JCC)C").is_none());
    assert!(site("compareAndExchangeByte", "(Ljava/lang/Object;JII)I").is_none());
    assert!(unsafe_inline_subword_site(
        "sun/misc/Unsafe",
        "compareAndExchangeByte",
        "(Ljava/lang/Object;JBB)B"
    )
    .is_none());
}

/// UNSAFE21-2: the CMPXCHG of the access width, then the witness
/// sign-extended or the `0` / `1`.
#[test]
fn subword_rmw_bytes() {
    let r = |width, cae| unsafe_inline_subword_rmw_bytes(UnsafeSubwordRmw { width, cae });
    assert_eq!(
        r(1, true),
        vec![0x49, 0x92, 0xF0, 0x43, 0x0F, 0xB0, 0x14, 0x1A, 0x48, 0x0F, 0xBE, 0xC0]
    );
    assert_eq!(
        r(2, true),
        vec![0x49, 0x92, 0xF0, 0x66, 0x43, 0x0F, 0xB1, 0x14, 0x1A, 0x48, 0x0F, 0xBF, 0xC0]
    );
    assert_eq!(
        r(2, false),
        vec![
            0x49, 0x92, 0xF0, 0x66, 0x43, 0x0F, 0xB1, 0x14, 0x1A, 0x0F, 0x94, 0xC0, 0x0F, 0xB6,
            0xC0
        ]
    );
    assert_eq!(
        r(1, false),
        vec![
            0x49, 0x92, 0xF0, 0x43, 0x0F, 0xB0, 0x14, 0x1A, 0x0F, 0x94, 0xC0, 0x0F, 0xB6, 0xC0
        ]
    );
}

/// UNSAFE21-2: the sub-word screen reads its own half (a `disp32` cell
/// load at `UNSAFE_SUBWORD_FIELD_CELL_BASE`) and keys a `short` access
/// with the low key bit.
#[test]
fn subword_screen_reads_its_half() {
    let (b1, e1) = unsafe_inline_subword_field_cell_bytes(0x1000, 0x2000, 1);
    let (b2, e2) = unsafe_inline_subword_field_cell_bytes(0x1000, 0x2000, 2);
    assert_eq!(e1.len(), e2.len());
    let mut load = vec![0x4F, 0x8B, 0x9C, 0xDA];
    load.extend_from_slice(&UNSAFE_SUBWORD_FIELD_CELL_BASE.to_le_bytes());
    for b in [&b1, &b2] {
        assert!(b.windows(load.len()).any(|w| w == &load[..]));
    }
    assert!(b2.windows(3).any(|w| w == [0x83, 0xCA, 0x01]), "OR EDX, 1");
    assert_eq!(
        unsafe_subword_field_arm_words(
            &DirectHelperTable::EMPTY,
            UnsafeSubwordRmw {
                width: 1,
                cae: true
            }
        ),
        None
    );
}
