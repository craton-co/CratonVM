use super::*;

fn site(base: FfmSegmentBase, accessor: FfmElementAccessor) -> FfmElementInlineSite {
    FfmElementInlineSite {
        shape: FfmSegmentShape {
            class_id: 9,
            length: 0,
            read_only: 1,
            scope: 2,
            base,
        },
        accessor,
        bail_on_decline: true,
    }
}


#[test]
fn the_access_plan_mirrors_the_jdk_alignment_rules() {
    let int_at = FfmElementAccessor {
        carrier: b'I',
        is_put: false,
        by_index: true,
    };
    let facts = |align: u32, be: bool| FfmLayoutFacts {
        carrier: b'I',
        width: 4,
        align,
        big_endian: be,
    };
    let native = site(FfmSegmentBase::Native { min: 3 }, int_at);
    assert_eq!(
        ffm_inline_access_plan(&native, facts(4, false)),
        Some(FfmInlineAccess {
            width: 4,
            signed: true,
            align_mask: 3,
            big_endian: false
        })
    );
    assert_eq!(
        ffm_inline_access_plan(&native, facts(1, true)).map(|p| (p.align_mask, p.big_endian)),
        Some((0, true)),
        "an unaligned layout owes no alignment guard"
    );
    // Wider than the width: getAtIndex raises.
    assert_eq!(ffm_inline_access_plan(&native, facts(8, false)), None);
    // Heap byte[]: maxAlignMask 1, so an aligned int layout always fails.
    let heap_b = site(
        FfmSegmentBase::Heap {
            base: 4,
            offset: 3,
            element: b'B',
        },
        int_at,
    );
    assert_eq!(ffm_inline_access_plan(&heap_b, facts(4, false)), None);
    assert!(ffm_inline_access_plan(&heap_b, facts(1, false)).is_some());
    // Heap int[]: an aligned int layout is served.
    let heap_i = site(
        FfmSegmentBase::Heap {
            base: 4,
            offset: 3,
            element: b'I',
        },
        int_at,
    );
    assert!(ffm_inline_access_plan(&heap_i, facts(4, false)).is_some());
    // Carrier mismatch.
    let mut long_facts = facts(4, false);
    long_facts.carrier = b'J';
    long_facts.width = 8;
    assert_eq!(ffm_inline_access_plan(&native, long_facts), None);
    // char is unsigned.
    let char_get = FfmElementAccessor {
        carrier: b'C',
        is_put: false,
        by_index: false,
    };
    let c = FfmLayoutFacts {
        carrier: b'C',
        width: 2,
        align: 2,
        big_endian: false,
    };
    assert_eq!(
        ffm_inline_access_plan(&site(FfmSegmentBase::Native { min: 3 }, char_get), c)
            .map(|p| p.signed),
        Some(false)
    );
}
