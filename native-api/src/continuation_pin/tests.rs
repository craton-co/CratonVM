use super::*;

#[test]
fn each_guard_releases_what_it_took() {
    assert_eq!((pins(), transparent_natives()), (0, 0));
    {
        let _p = pin();
        let _t = transparent();
        let _t2 = transparent();
        assert_eq!((pins(), transparent_natives()), (1, 2));
    }
    assert_eq!((pins(), transparent_natives()), (0, 0));
}

/// gci i18/vp: `tail_call` declares exactly one transparent frame for the
/// call, hands the call's answer back as is, and withdraws the
/// declaration afterwards -- on an `Err` answer too.
#[test]
fn gci_i18vp_tail_call_is_one_transparent_frame_for_the_call_only() {
    assert_eq!(transparent_natives(), 0);
    let seen = tail_call(transparent_natives);
    assert_eq!(seen, 1, "one frame declared while the call runs");
    assert_eq!(transparent_natives(), 0, "withdrawn after the call");
    let failed: Result<u32, &str> = tail_call(|| {
        assert_eq!(transparent_natives(), 1);
        Err("yield")
    });
    assert_eq!(failed, Err("yield"), "the answer is handed back unchanged");
    assert_eq!(transparent_natives(), 0, "withdrawn on the error path too");
    // Nested tail calls (a transparent native whose body reaches another
    // one) count one frame each.
    let inner = tail_call(|| tail_call(transparent_natives));
    assert_eq!(inner, 2);
    assert_eq!(transparent_natives(), 0);
}

/// Interpreter round i6 wave 130, lane L6: a carrier slice's swap starts
/// from `(0, 0)` and puts the carrier's pair back.
#[test]
fn i6w130_replace_counts_sets_aside_and_restores() {
    let _p = pin();
    let _t = transparent();
    let carrier = replace_counts(0, 0);
    assert_eq!(carrier, (1, 1));
    assert_eq!((pins(), transparent_natives()), (0, 0));
    {
        let _slice_pin = pin();
        assert_eq!(pins(), 1);
    }
    assert_eq!(replace_counts(carrier.0, carrier.1), (0, 0));
    assert_eq!((pins(), transparent_natives()), (1, 1));
}
