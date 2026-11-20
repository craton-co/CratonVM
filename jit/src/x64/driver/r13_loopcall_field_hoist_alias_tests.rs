use super::{
    field_hoist_aliases_for_bce, ArrayLenHoist, FieldHoist, IntFieldHoist, LoopHoist,
    FIELD_HOIST_NO_INDEX,
};

fn x2(header: usize, end: usize, sites: Vec<(usize, usize)>) -> LoopHoist {
    LoopHoist {
        loop_header: header,
        loop_end: end,
        seq_start: sites[0].0,
        seq_end: sites[0].1,
        array_local: 0,
        index_local: FIELD_HOIST_NO_INDEX,
        field: Some(FieldHoist {
            field_off: 16,
            field_index: 0,
            sites,
        }),
    }
}

fn int_hoist(header: usize, end: usize, sites: Vec<(usize, usize)>) -> ArrayLenHoist {
    ArrayLenHoist {
        loop_header: header,
        loop_end: end,
        array_local: 0,
        trap_bci: sites[0].1 - 3,
        sites,
        int_field: Some(IntFieldHoist {
            field_off: 20,
            field_index: 1,
        }),
    }
}

/// Array hoists first, then `int` ones, numbered from `max_locals`; an
/// `aaload` hoist and a length hoist are not aliases; a synthetic local
/// that would reach 64 is not handed out.
#[test]
fn aliases_are_numbered_from_max_locals_and_stop_below_64() {
    let aaload = LoopHoist {
        loop_header: 4,
        loop_end: 28,
        seq_start: 7,
        seq_end: 10,
        array_local: 1,
        index_local: 2,
        field: None,
    };
    let length = ArrayLenHoist {
        loop_header: 4,
        loop_end: 28,
        array_local: 1,
        sites: vec![(5, 7)],
        trap_bci: 6,
        int_field: None,
    };
    let hoists = vec![aaload, x2(4, 28, vec![(12, 16), (20, 24)])];
    let lens = vec![length, int_hoist(4, 28, vec![(8, 12)])];
    let aliases = field_hoist_aliases_for_bce(&hoists, &lens, 3);
    assert_eq!(aliases.len(), 2);
    assert_eq!(
        (aliases[0].key, aliases[0].synthetic, aliases[0].is_array),
        (12, 3, true)
    );
    assert_eq!(aliases[0].sites, vec![(12, 16), (20, 24)]);
    assert_eq!(
        (aliases[1].key, aliases[1].synthetic, aliases[1].is_array),
        (8, 4, false)
    );
    assert_eq!((aliases[1].loop_header, aliases[1].loop_end), (4, 28));

    // `max_locals` 63: only the first fits.
    let aliases = field_hoist_aliases_for_bce(&hoists, &lens, 63);
    assert_eq!(aliases.len(), 1);
    assert_eq!(aliases[0].synthetic, 63);
    assert!(field_hoist_aliases_for_bce(&hoists, &lens, 64).is_empty());
}
