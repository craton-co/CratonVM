use super::tlab_gate_bump_probe;

/// OFF is the old gate exactly; ON probes for the floor, raised to the
/// object in hand and capped at the request.
#[test]
fn the_gate_probes_the_full_request_unless_opted_in() {
    let k = 1024;
    for (floor, object, requested) in [(256, 48, 256 * k), (256, 24 * k, 8 * k), (8, 16, 8)] {
        assert_eq!(
            tlab_gate_bump_probe(false, floor, object, requested),
            requested
        );
    }
    assert_eq!(
        tlab_gate_bump_probe(true, 256, 48, 256 * k),
        256,
        "the floor"
    );
    assert_eq!(
        tlab_gate_bump_probe(true, 256, 16 * k, 256 * k),
        16 * k,
        "the object"
    );
    assert_eq!(
        tlab_gate_bump_probe(true, 256, 24 * k, 8 * k),
        8 * k,
        "never above the request"
    );
}
