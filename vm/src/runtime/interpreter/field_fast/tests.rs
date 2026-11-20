use super::*;

/// zt t18/zb1 (zr1br A2a): an ARMED read barrier turns the quickened arms
/// off for the frame (they read reference slots raw), and disarming turns
/// them back on. Skipped when this environment already keeps them off.
#[test]
fn zt18zb1_an_armed_read_barrier_keeps_the_quickened_arms_off() {
    // zt t19/x15: the arming flips the process-wide gate.
    let _gate = crate::ZGC_READ_GATE_TEST_LOCK.lock();
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.mem.heap.is_zgc() {
        return; // not a ZGC build/default
    };
    if fast_field_zgc(&shared).is_none() {
        return; // a kill switch or diagnostic in this environment
    }
    assert!(shared.mem.heap.zgc_arm_load_barrier_plain_stores(false));
    let armed = fast_field_zgc(&shared).is_none();
    shared.mem.heap.zgc_set_barrier_color(None);
    let disarmed = fast_field_zgc(&shared).is_some();
    assert!(armed, "armed: the full handlers serve");
    assert!(disarmed, "disarmed: the quickened arms are back");
}

/// The `*aload` / `*astore` table is hand-written from JVMS §6.5, and a
/// transcription error in it would read or write an element at the wrong
/// width — silently, on a live array. Pin every entry against the opcode
/// numbers and the `ArrayElementType` widths the header carries.
#[test]
fn the_primitive_array_opcode_table_matches_the_jvms_element_types() {
    use ArrayElementType::*;
    // (load opcode, store opcode, element type, byte width)
    let expected: &[(u8, u8, ArrayElementType, usize)] = &[
        (0x2e, 0x4f, Int, 4),    // iaload  / iastore
        (0x2f, 0x50, Long, 8),   // laload  / lastore
        (0x30, 0x51, Float, 4),  // faload  / fastore
        (0x31, 0x52, Double, 8), // daload  / dastore
        (0x33, 0x54, Byte, 1),   // baload  / bastore
        (0x34, 0x55, Char, 2),   // caload  / castore
        (0x35, 0x56, Short, 2),  // saload  / sastore
    ];
    for &(load, store, et, width) in expected {
        assert_eq!(
            prim_elem_for_opcode(load),
            Some((et, width)),
            "load opcode {load:#x}"
        );
        assert_eq!(
            prim_elem_for_opcode(store),
            Some((et, width)),
            "store opcode {store:#x}"
        );
    }
}

/// `aaload` (0x32) and `aastore` (0x53) must NOT be in the table: a
/// reference element needs the load barrier and the array-store check,
/// which only the general path performs.
#[test]
fn the_reference_array_opcodes_are_not_quickened() {
    assert_eq!(prim_elem_for_opcode(0x32), None, "aaload");
    assert_eq!(prim_elem_for_opcode(0x53), None, "aastore");
    // And nothing outside the two ranges answers either.
    for op in [0x00u8, 0x2d, 0x36, 0x4e, 0x57, 0xb4, 0xff] {
        assert_eq!(prim_elem_for_opcode(op), None, "opcode {op:#x}");
    }
}

fn site_with(desc_byte: u8) -> FastFieldSite {
    FastFieldSite {
        receiver_class_id: ClassId::new(1),
        num_slots: 1,
        offset: 0,
        storage: None,
        field_index: 0,
        desc_byte,
        is_reference: matches!(desc_byte, b'L' | b'['),
        final_put: FastFieldSite::PUT_ANY,
    }
}

fn resolved(declaring: u32, is_final: bool) -> ResolvedField {
    ResolvedField {
        declaring_class_id: ClassId::new(declaring),
        field_index: 0,
        is_static: false,
        is_volatile: false,
        is_final,
        is_reference: false,
        desc_byte: b'I',
    }
}

/// The fast arm's final-field rule (JVMS §6.5 `putfield`): a non-final
/// field is open to every store, a final field of the site's own class
/// only to `<init>`, and a final field of another class to none — even
/// when a legal `getfield` through the same constant-pool entry filled
/// the site.
#[test]
fn final_put_rule_keeps_illegal_final_stores_on_the_full_handler() {
    let own = ClassId::new(7);
    assert_eq!(
        final_put_rule(&resolved(7, false), own),
        FastFieldSite::PUT_ANY
    );
    assert_eq!(
        final_put_rule(&resolved(9, false), own),
        FastFieldSite::PUT_ANY
    );
    assert_eq!(
        final_put_rule(&resolved(7, true), own),
        FastFieldSite::PUT_INIT_ONLY
    );
    assert_eq!(
        final_put_rule(&resolved(9, true), own),
        FastFieldSite::PUT_NEVER
    );
}

/// The trivial-getter shortcut answers a getter without building its
/// frame, so its return opcode must agree with the field's type exactly as
/// `try_execute_cached_trivial_instance_getter` requires — otherwise a
/// `long` field could be returned through `ireturn`.
#[test]
fn the_trivial_getter_return_opcode_agrees_only_with_its_own_field_type() {
    const IRETURN: u8 = 0xac;
    const LRETURN: u8 = 0xad;
    const FRETURN: u8 = 0xae;
    const DRETURN: u8 = 0xaf;
    const ARETURN: u8 = 0xb0;
    let accepted: &[(u8, u8)] = &[
        (b'J', LRETURN),
        (b'F', FRETURN),
        (b'D', DRETURN),
        (b'L', ARETURN),
        (b'[', ARETURN),
        (b'Z', IRETURN),
        (b'B', IRETURN),
        (b'C', IRETURN),
        (b'S', IRETURN),
        (b'I', IRETURN),
    ];
    for &(desc, ret) in accepted {
        assert!(
            return_opcode_agrees(&site_with(desc), ret),
            "{} should be returnable by {ret:#x}",
            desc as char
        );
    }
    // Every other pairing is refused.
    let all_returns = [IRETURN, LRETURN, FRETURN, DRETURN, ARETURN];
    for &(desc, ret) in accepted {
        for other in all_returns {
            if other == ret {
                continue;
            }
            assert!(
                !return_opcode_agrees(&site_with(desc), other),
                "{} must NOT be returnable by {other:#x}",
                desc as char
            );
        }
    }
}
