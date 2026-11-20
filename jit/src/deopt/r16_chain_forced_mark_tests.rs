//! Round 16 lane chain (CH5W4-1 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): the
//! forced bit rides in the stash. The forced pad tags the point pointer,
//! the trampoline strips the tag and marks the entry, a take remembers the
//! mark and a restash of the same point puts it back; a claim and the
//! uncharged decision each count once per trip.
use super::*;

fn point() -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0x40,
        bci: 7,
        reason: DeoptReason::ReceiverTypeChanged,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "P.m:(I)V".to_string(),
            bci: 7,
            locals: vec![FrameValue::Int(5)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    }
}

fn on_a_fresh_thread(body: impl FnOnce() + Send + 'static) {
    std::thread::spawn(body).join().expect("test body panicked");
}

#[test]
fn a_tagged_pad_stashes_the_untagged_point_marked_forced() {
    on_a_fresh_thread(|| {
        let p = Box::new(point());
        let addr = &*p as *const DeoptimizationPoint as usize;
        let tagged = (addr | FORCED_CHAIN_PAD_TAG as usize) as *const DeoptimizationPoint;
        assert_eq!(ir_deopt_entry(tagged, 0, std::ptr::null()), i64::MIN);
        let (f, cause, at) = take_last_deopt_with_point().expect("stashed");
        assert_eq!((f.method_key.as_str(), f.bci), ("P.m:(I)V", 7));
        assert_eq!(at, addr, "the stash names the untagged point");
        assert_eq!(
            cause.map(|c| c.reason),
            Some(DeoptReason::ReceiverTypeChanged)
        );
        assert_eq!(
            claim_forced_trip_mark(0),
            ForcedTripMark::default(),
            "no point"
        );
        assert!(!claim_forced_trip_mark(addr + 8).forced, "another point");
        assert_eq!(
            claim_forced_trip_mark(addr),
            ForcedTripMark {
                forced: true,
                first_claim: true
            }
        );
        assert_eq!(
            claim_forced_trip_mark(addr),
            ForcedTripMark {
                forced: true,
                first_claim: false
            },
            "a second sink asking about the same trip does not recount it"
        );
        assert!(mark_taken_forced_trip_uncharged());
        assert!(
            !mark_taken_forced_trip_uncharged(),
            "uncharged counted once per trip"
        );

        // The same point, untagged: a natural failure, never forced.
        assert_eq!(ir_deopt_entry(&*p, 0, std::ptr::null()), i64::MIN);
        let (_, _, at) = take_last_deopt_with_point().expect("stashed");
        assert_eq!(at, addr);
        assert_eq!(claim_forced_trip_mark(addr), ForcedTripMark::default());
        assert!(
            !mark_taken_forced_trip_uncharged(),
            "a natural trap is never uncharged"
        );
    });
}

/// Round 16 wave 14 (lane framestate, CH16-2): a forced trip's resume is
/// credited to its point once per trip, survives a restash of the same
/// point, and a natural trap is never credited.
#[test]
fn a_forced_trips_resume_is_marked_once_per_trip() {
    on_a_fresh_thread(|| {
        let p = Box::new(point());
        let addr = &*p as *const DeoptimizationPoint as usize;
        let tagged = (addr | FORCED_CHAIN_PAD_TAG as usize) as *const DeoptimizationPoint;
        assert_eq!(ir_deopt_entry(tagged, 0, std::ptr::null()), i64::MIN);
        let (f, cause, at) = take_last_deopt_with_point().expect("stashed");
        assert!(claim_forced_trip_mark(at).forced);
        assert_eq!(mark_taken_forced_trip_resumed(), Some(addr));
        assert_eq!(mark_taken_forced_trip_resumed(), None, "once per trip");
        // Put back and taken again (a sink restashing): still credited.
        restash_last_deopt_with_point(f, cause, at);
        let _ = take_last_deopt_with_point().expect("restashed");
        assert_eq!(mark_taken_forced_trip_resumed(), None, "the same trip");

        // A new forced trip at the same point is a new credit.
        assert_eq!(ir_deopt_entry(tagged, 0, std::ptr::null()), i64::MIN);
        let _ = take_last_deopt_with_point().expect("stashed");
        assert_eq!(mark_taken_forced_trip_resumed(), Some(addr));

        // A natural trap is never credited.
        assert_eq!(ir_deopt_entry(&*p, 0, std::ptr::null()), i64::MIN);
        let _ = take_last_deopt_with_point().expect("stashed");
        assert_eq!(mark_taken_forced_trip_resumed(), None);
    });
}

#[test]
fn a_restash_of_the_same_point_keeps_the_mark_and_its_claim() {
    on_a_fresh_thread(|| {
        let p = Box::new(point());
        let addr = &*p as *const DeoptimizationPoint as usize;
        let tagged = (addr | FORCED_CHAIN_PAD_TAG as usize) as *const DeoptimizationPoint;
        assert_eq!(ir_deopt_entry(tagged, 0, std::ptr::null()), i64::MIN);
        // The call-site service: take, claim, refuse, restash.
        let (f, cause, at, source) = take_last_deopt_with_source().expect("stashed");
        assert!(claim_forced_trip_mark(at).first_claim);
        restash_last_deopt_with_source(f, cause, at, source);
        // The outer door takes it as a foreign frame: still forced (G2 of
        // `r15w4-chain5-review-findings`), already counted.
        let (f, cause, at) = take_last_deopt_with_point().expect("restashed");
        assert_eq!(
            claim_forced_trip_mark(at),
            ForcedTripMark {
                forced: true,
                first_claim: false
            }
        );
        // A restash under a different point address (0: the plain one)
        // carries no mark.
        restash_last_deopt_with_point(f, cause, 0);
        let (_, _, at0) = take_last_deopt_with_point().expect("restashed");
        assert_eq!(at0, 0);
        assert!(!claim_forced_trip_mark(addr).forced);
    });
}
