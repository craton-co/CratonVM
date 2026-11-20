    use super::{
        door_rerun_census, frameless_rerun_is_exact, frameless_rerun_is_exact_for_callee,
        note_door_rerun, note_door_rerun_refused, unsound_rerun_refused, DoorRerunCause,
    };
    use crate::jit::helpers::FramelessTrapSite;

    /// `putstatic` at bci 1, then a storing loop (the wave-4 fixture).
    const STORE_THEN_LOOP: [u8; 23] = [
        0x04, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0x05, 0xb3, 0x00,
        0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];

    fn switch_on(name: &str) -> bool {
        cratonvm_types::flags::runtime_flag_default_on(name)
    }

#[test]
    fn a_refusal_moves_the_count_out_of_the_unsound_column() {
        let row = |name: &str| {
            door_rerun_census()
                .into_iter()
                .find(|(n, _, _)| *n == name)
                .map(|(_, s, u)| (s, u))
        };
        assert!(
            row("callee-exception").is_some(),
            "the callee-exception row"
        );
        let (refused_before, refused_unsound) = row("refused").unwrap_or((0, 0));
        assert_eq!(refused_unsound, 0, "a refusal is never a double commit");
        note_door_rerun(DoorRerunCause::CalleeException, false);
        note_door_rerun_refused(DoorRerunCause::CalleeException);
        let (refused_after, _) = row("refused").unwrap_or((0, 0));
        // Other tests of this binary may only add to the row.
        assert!(refused_after > refused_before);
    }


    // Round 13 wave 13 (lane jitfix;
    // `r13w12-replay6-callee-service-trusts-any-stubs-exact-stamp`): a
    // call-site service re-running method Y does not take the `exact` stamp
    // of a stub method X raised -- Y's storing body gets the whole-body rule
    // -- while Y's own stub, and a stub whose method is unknown, still answer.
