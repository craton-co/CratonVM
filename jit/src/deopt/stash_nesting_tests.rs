use super::*;

fn frame(key: &str, bci: u32, obj: u64) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        bci,
        locals: vec![FrameValue::Object(obj)],
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics: ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    }
}

/// Each test gets its own thread, because the stashes are thread-locals and
/// the counters they bump are process-wide.
fn on_a_fresh_thread(body: impl FnOnce() + Send + 'static) {
    std::thread::spawn(body).join().expect("test thread");
}

/// HANDLER11-1 (JIT round 19 wave 11, lane handler11; page
/// `r19-outside-manager-c2-upgrade-crashes-under-load-FIXED-20261012.md`): a door
/// that entered its body at a push mark takes only a frame pushed after
/// it. The shape of the `R16LocaleWide` foreign-stash `InternalError`: a
/// frame of `Locale.isUnicodeExtensionKey` left on the stash by an earlier
/// trap no consumer took, and a later `Locale.forLanguageTag` door whose
/// body returned a bare sentinel took it as its own body's trap.
#[test]
fn a_door_takes_only_a_frame_pushed_after_its_entry_mark() {
    on_a_fresh_thread(|| {
        // An orphan from an earlier trap.
        let orphan = "java/util/Locale.isUnicodeExtensionKey:(Ljava/lang/String;)Z";
        LAST_DEOPT.with(|c| push_stash(c, frame(orphan, 1, 0x7), false));
        let mark = deopt_stash_push_mark();
        let dropped_before = deopt_stash_nesting_counts().1;
        // The body returned a bare sentinel: nothing of its own is stashed.
        let (taken, discarded) = take_last_deopt_with_point_pushed_after(mark);
        assert!(taken.is_none(), "the orphan is not the body's trap");
        assert_eq!(
            discarded, 1,
            "the orphan is discarded, not left for a later sink"
        );
        assert!(!has_last_deopt());
        assert!(
            deopt_stash_nesting_counts().1 > dropped_before,
            "the discard is counted"
        );

        // A body's own trap, pushed after the mark, over another orphan.
        LAST_DEOPT.with(|c| push_stash(c, frame("Old.o:()V", 2, 0x10), false));
        let mark = deopt_stash_push_mark();
        LAST_DEOPT.with(|c| push_stash(c, frame("Own.m:()V", 5, 0x20), false));
        // A service inside the body took it and put it back: still the
        // body's (a restash keeps the push number).
        let (f, cause, point) = take_last_deopt_with_point().expect("own frame");
        restash_last_deopt_with_point(f, cause, point);
        let (taken, discarded) = take_last_deopt_with_point_pushed_after(mark);
        assert_eq!(
            taken.map(|(f, _, _)| f.method_key),
            Some("Own.m:()V".to_string())
        );
        assert_eq!(discarded, 0);
        assert!(
            !has_last_deopt(),
            "the take discards what lay beneath, as before"
        );

        // An orphan a service took and restashed inside the body stays an
        // orphan: the restash keeps its OLD number.
        LAST_DEOPT.with(|c| push_stash(c, frame("Old.p:()V", 3, 0x30), false));
        let mark = deopt_stash_push_mark();
        let old = take_last_deopt().expect("orphan");
        restash_last_deopt(old);
        let (taken, discarded) = take_last_deopt_with_point_pushed_after(mark);
        assert!(taken.is_none());
        assert_eq!(discarded, 1);
    });
}

/// HANDLER11-3 (lane handler12): a consumer that completes a call over the
/// entry it saw on entry stamps it as left (and drops it on request); an
/// entry pushed after that, or nothing stashed, is not its orphan.
#[test]
fn a_completing_consumer_stamps_and_drops_the_entry_it_left() {
    on_a_fresh_thread(|| {
        assert!(!note_stash_left_by(deopt_stash_top_seq(), "empty", || true));
        LAST_DEOPT.with(|c| push_stash(c, frame("Callee.c:()I", 4, 0x40), false));
        let top = deopt_stash_top_seq();
        assert!(note_stash_left_by(top, "callee-deopt-service", || false));
        assert_eq!(
            LAST_DEOPT.with(|c| c.borrow().last().and_then(|e| e.left_by)),
            Some("callee-deopt-service")
        );
        // A newer entry is not the one this consumer saw.
        LAST_DEOPT.with(|c| push_stash(c, frame("Other.o:()V", 1, 0x50), false));
        assert!(!note_stash_left_by(top, "callee-deopt-service", || true));
        assert!(has_last_deopt());
        let newer = deopt_stash_top_seq();
        assert!(note_stash_left_by(newer, "door-throw", || true));
        assert!(!has_last_deopt(), "dropped, with what lay beneath");
    });
}

/// The nested case: a trap's frame is left unclaimed, and a later,
/// unrelated trap stashes over it. The fresh frame is what the sink takes;
/// the stale one is not silently destroyed at the moment the fresh one
/// arrives (its oops stay visible to the collector), and its discard at
/// the take is counted.
#[test]
fn a_nested_stash_does_not_destroy_the_older_frame_on_arrival() {
    on_a_fresh_thread(|| {
        // The counters are process-wide and other tests run concurrently,
        // so assert that they MOVED, never on an exact value.
        let before = deopt_stash_nesting_counts();
        restash_last_deopt(frame("Callee.b:()V", 3, 0x1000));
        LAST_DEOPT.with(|c| push_stash(c, frame("Caller.a:()V", 9, 0x2000), false));
        assert!(
            deopt_stash_nesting_counts().0 > before.0,
            "stashing over a non-empty stash is counted"
        );
        // BOTH frames' oops are offered to the collector while they are
        // stashed — the single slot could only ever offer one.
        let mut seen = Vec::new();
        for_each_stashed_deopt_object(|a| seen.push(a));
        seen.sort_unstable();
        assert_eq!(seen, vec![0x1000, 0x2000]);

        // The sink still gets the frame of the trap it is servicing.
        assert_eq!(
            peek_last_deopt_identity(),
            Some(("Caller.a:()V".to_string(), 9))
        );
        let taken = take_last_deopt().expect("the fresh frame");
        assert_eq!(taken.method_key, "Caller.a:()V");
        // And the stale frame is discarded with it, counted, rather than
        // left to be mistaken for a later trap.
        assert!(deopt_stash_nesting_counts().1 > before.1);
        assert!(take_last_deopt().is_none(), "nothing may be left behind");
    });
}

/// gen r4w6/oomjit6: the entry-floor truncation drops exactly the frames
/// at or above the floor, keeps the ones below it (an outer sink's), and
/// is a no-op at or above the current depth.
#[test]
fn exceptional_stash_truncation_keeps_the_frames_below_the_floor() {
    on_a_fresh_thread(|| {
        assert_eq!(exceptional_stash_depth(), 0);
        // An outer sink's frame, stashed before the compiled activation
        // was entered: the floor the door records is 1.
        restash_exceptional_frame(frame("Sink.outer:()V", 5, 0x100));
        let floor = exceptional_stash_depth();
        assert_eq!(floor, 1);
        // Two callee frames published on their way out to a compiled
        // catch inside that activation.
        restash_exceptional_frame(frame("Callee.c:()V", 7, 0x200));
        restash_exceptional_frame(frame("Callee.d:()V", 9, 0x300));
        assert_eq!(exceptional_stash_depth(), 3);
        assert_eq!(truncate_exceptional_stash_to(floor), 2);
        assert_eq!(exceptional_stash_depth(), 1);
        let mut seen = Vec::new();
        for_each_stashed_deopt_object(|a| seen.push(a));
        assert_eq!(
            seen,
            vec![0x100],
            "only the outer sink's frame stays rooted"
        );
        assert_eq!(
            peek_exceptional_identity(),
            Some(("Sink.outer:()V".to_string(), 5))
        );
        // A floor at or above the depth drops nothing.
        assert_eq!(truncate_exceptional_stash_to(1), 0);
        assert_eq!(truncate_exceptional_stash_to(usize::MAX), 0);
        assert_eq!(exceptional_stash_depth(), 1);
        assert_eq!(truncate_exceptional_stash_to(0), 1);
        assert!(take_exceptional_frame().is_none());
    });
}

/// Interpreter round i1 wave 25, lane L2: the claiming take pops only the
/// top frame, only above the floor, only when the claim accepts it, and
/// never discards what is beneath.
#[test]
fn a_claiming_take_pops_only_an_accepted_top_frame_above_the_floor() {
    on_a_fresh_thread(|| {
        restash_exceptional_frame_with_point(frame("Sink.outer:()V", 5, 0x100), None, 0x10);
        let floor = exceptional_stash_depth();
        restash_exceptional_frame_with_point(frame("M.m:()V", 7, 0x200), None, 0x20);
        restash_exceptional_frame_with_point(frame("M.m:()V", 9, 0x300), None, 0x20);
        // A refused claim leaves everything in place.
        assert!(take_exceptional_frame_above_if(floor, |_, p| p == 0x99).is_none());
        assert_eq!(exceptional_stash_depth(), 3);
        let claim = |f: &ReconstructedFrame, p: usize| f.method_key == "M.m:()V" && p == 0x20;
        let (top, _, point) =
            take_exceptional_frame_above_if(floor, claim).expect("the top frame");
        assert_eq!((top.bci, point), (9, 0x20));
        assert_eq!(exceptional_stash_depth(), 2, "the frame beneath is kept");
        let (next, _, _) = take_exceptional_frame_above_if(floor, claim).expect("the next one");
        assert_eq!(next.bci, 7);
        // At the floor nothing is taken, even a frame the claim would accept.
        assert!(take_exceptional_frame_above_if(floor, |_, _| true).is_none());
        assert_eq!(
            peek_exceptional_identity(),
            Some(("Sink.outer:()V".to_string(), 5))
        );
        assert_eq!(truncate_exceptional_stash_to(0), 1);
    });
}

/// JIT round 17 wave 4, lane stashdrop: the discards-keeping take pops the
/// top frame like `take_exceptional_frame_with_point`, empties the stash
/// the same way, counts the discards the same way, and hands them back
/// outermost first with their points.
#[test]
fn a_take_hands_back_the_frames_it_discards() {
    on_a_fresh_thread(|| {
        assert!(take_exceptional_frame_with_point_and_discards().is_none());
        restash_exceptional_frame_with_point(frame("A.a:()V", 1, 0x100), None, 0x10);
        restash_exceptional_frame_with_point(frame("B.b:()V", 2, 0x200), None, 0x20);
        restash_exceptional_frame_with_point(frame("C.c:()V", 3, 0x300), None, 0x30);
        let before = deopt_stash_nesting_counts().1;
        let (top, _, point, discarded) =
            take_exceptional_frame_with_point_and_discards().expect("the top frame");
        assert_eq!((top.method_key.as_str(), point), ("C.c:()V", 0x30));
        let seen: Vec<(&str, usize)> = discarded
            .iter()
            .map(|(f, p)| (f.method_key.as_str(), *p))
            .collect();
        assert_eq!(seen, vec![("A.a:()V", 0x10), ("B.b:()V", 0x20)]);
        assert_eq!(
            exceptional_stash_depth(),
            0,
            "as empty as after a plain take"
        );
        // Process-wide counter: other tests may add to it concurrently.
        assert!(deopt_stash_nesting_counts().1 >= before + 2);
        // A lone frame discards nothing.
        restash_exceptional_frame_with_point(frame("D.d:()V", 4, 0x400), None, 0x40);
        let (_, _, _, none) =
            take_exceptional_frame_with_point_and_discards().expect("the lone frame");
        assert!(none.is_empty());
    });
}

/// N2 — the dispatch-helper resume arm asks *is this the method I just
/// invoked?*, and for an inlined chain the answer is on the OUTERMOST
/// scope, not on the stash's own `method_key`.
///
/// `try_resume_trapped_callee` compares the peeked name against the call
/// site's `info.method_name`. A spliced callee's name is never the call
/// site's name, so peeking the innermost refuses every chain by
/// construction and leaves the frame travelling outward as an orphan —
/// which is the shape `jit-direct-call-mints-an-orphaned-deopt-frame` is
/// about. The two peeks agree exactly when there is no chain, which is
/// every frame in the tree today.
#[test]
fn the_outermost_peek_names_the_method_the_call_site_invoked() {
    on_a_fresh_thread(|| {
        // No chain: the two peeks are the same answer.
        restash_last_deopt(frame("A.m:()V", 1, 0x10));
        assert_eq!(
            peek_last_deopt_identity(),
            peek_last_deopt_outermost_identity()
        );
        let _ = take_last_deopt();

        // A chain. `caller_frames` runs innermost-first, so the OUTERMOST
        // scope — the one the artifact belongs to and the one the compiled
        // caller actually invoked — is `last()`.
        let mut f = frame("Callee.spliced:()V", 3, 0x20);
        f.caller_frames = vec![
            frame("Mid.inlined:()V", 11, 0x21),
            frame("Outer.entry:()V", 42, 0x22),
        ];
        restash_last_deopt(f);
        assert_eq!(
            peek_last_deopt_identity(),
            Some(("Callee.spliced:()V".to_string(), 3)),
            "the innermost peek still answers about the trapping scope"
        );
        assert_eq!(
            peek_last_deopt_outermost_identity(),
            Some(("Outer.entry:()V".to_string(), 42)),
            "and the outermost peek answers about the method that was invoked"
        );
        let _ = take_last_deopt();
    });
}

/// A deopt point carrying `semantics`, whose frame state is plain values
/// (nothing to resolve against a machine state).
fn point_with(semantics: ResumeSemantics) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0x40,
        bci: 7,
        reason: DeoptReason::NullCheck,
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
        semantics,
    }
}

/// The ordinary stash admits re-execute points only — the invariant that
/// makes every frame under the top a stale leftover rather than a live
/// callee, and so what makes discarding it (and resuming the top frame at
/// an invoke) correct.
///
/// A resume-after point (a caller parked mid-invoke) is turned into the
/// identity-less re-run sentinel every sink refuses, and counted. Through
/// the real trampoline entry, so a later producer cannot route around the
/// check.
///
/// A RETHROW point does not reach this gate at all, and that is asserted
/// here too rather than left to the sibling test: since round 10 wave 9
/// both trampoline entries route on `semantics.rethrow_exception` (not on
/// `reason == PendingException`), so a flagged point goes to
/// [`LAST_EXCEPTIONAL`] with its precise locals intact. Under the old
/// reason-keyed routing this same point — flagged, but carrying a
/// non-reason-9 `reason` — reached `ordinary_stash_frame` and lost those
/// locals to the sentinel, which is the defect
/// `an_exceptional_point_reaches_the_exceptional_stash_from_either_entry`
/// records for the entry that used to have it. `DeoptVerifier`'s
/// exception-state-agreement lane refuses such a disagreeing point at
/// install time, so this shape is reachable only by hand, as here.
#[test]
fn only_a_reexecute_point_enters_the_ordinary_stash() {
    on_a_fresh_thread(|| {
        let reexecute = point_with(ResumeSemantics::REEXECUTE);
        assert_eq!(ir_deopt_entry(&reexecute, 0, std::ptr::null()), i64::MIN);
        let f = take_last_deopt().expect("stashed");
        assert_eq!((f.method_key.as_str(), f.bci), ("P.m:(I)V", 7));
        assert_eq!(f.locals, vec![FrameValue::Int(5)]);

        let before = deopt_stash_non_reexecute_refused();
        let resume = point_with(ResumeSemantics::RESUME);
        assert_eq!(ir_deopt_entry(&resume, 0, std::ptr::null()), i64::MIN);
        let f = take_last_deopt().expect("the sentinel is stashed, not nothing");
        assert!(
            f.method_key.is_empty() && f.bci == u32::MAX,
            "RESUME must stash the re-run sentinel, got {} bci={}",
            f.method_key,
            f.bci
        );
        assert!(deopt_stash_non_reexecute_refused() > before, "and count it");

        let before = deopt_stash_non_reexecute_refused();
        let rethrow = point_with(ResumeSemantics::RETHROW);
        assert_eq!(ir_deopt_entry(&rethrow, 0, std::ptr::null()), i64::MIN);
        assert!(
            take_last_deopt().is_none(),
            "a RETHROW point is routed away from the ordinary stash entirely"
        );
        let f = take_exceptional_frame().expect("the exceptional stash holds it");
        assert_eq!(
            f.locals,
            vec![FrameValue::Int(5)],
            "and it keeps the precise locals the handler reads"
        );
        assert_eq!(
            deopt_stash_non_reexecute_refused(),
            before,
            "nothing was refused — it never reached the gate"
        );
    });
}

/// The trampoline keeps the point's own reason and speculation id beside
/// the frame (r11-tier-deopt-stash-drops-the-trap-reason), so a sink need
/// not re-derive them from a bci two points may share.
#[test]
fn the_stash_keeps_the_cause_of_the_point_that_fired() {
    on_a_fresh_thread(|| {
        let id = speculation_id(7, DeoptReason::BoundsCheck);
        let mut bounds = point_with(ResumeSemantics::REEXECUTE);
        bounds.reason = DeoptReason::BoundsCheck;
        bounds.speculation_id = id;
        assert_eq!(ir_deopt_entry(&bounds, 0, std::ptr::null()), i64::MIN);
        let want = DeoptCause {
            reason: DeoptReason::BoundsCheck,
            speculation_id: id,
        };
        assert_eq!(peek_last_deopt_cause(), Some(want));
        let (f, cause) = take_last_deopt_with_cause().expect("stashed");
        assert_eq!(f.bci, 7);
        assert_eq!(cause, Some(want));

        // Take-inspect-restash keeps the cause; the plain restash claims none.
        restash_last_deopt_with_cause(f, cause);
        assert_eq!(peek_last_deopt_cause(), Some(want));
        let f = take_last_deopt().expect("restashed");
        restash_last_deopt(f);
        assert_eq!(peek_last_deopt_cause(), None);
        let _ = take_last_deopt();

        // A null point has no cause to report.
        assert_eq!(
            ir_deopt_entry(std::ptr::null(), 0, std::ptr::null()),
            i64::MIN
        );
        let (f, cause) = take_last_deopt_with_cause().expect("the re-run sentinel");
        assert_eq!(f.bci, u32::MAX);
        assert_eq!(cause, None);
    });
}

/// The stash names the POINT that fired, so a sink holding the published
/// artifact can tell its own trap from one of a body it superseded
/// (r11w6-lower-deopt-cause-artifact-identity-patch).
#[test]
fn the_stash_names_the_point_and_an_artifact_owns_only_its_own_points() {
    on_a_fresh_thread(|| {
        let mut live = crate::CompiledMethod::new(
            crate::ExecutableBuffer::new(64).expect("alloc executable"),
        );
        live._deopt_point_boxes
            .push(Box::new(point_with(ResumeSemantics::REEXECUTE)));
        let retired = point_with(ResumeSemantics::REEXECUTE);
        let live_point: *const DeoptimizationPoint = &*live._deopt_point_boxes[0];
        let live_addr = live_point as usize;
        let retired_addr = &retired as *const DeoptimizationPoint as usize;

        assert_eq!(peek_last_deopt_point(), 0, "nothing stashed");
        assert_eq!(ir_deopt_entry(live_point, 0, std::ptr::null()), i64::MIN);
        // The peek names the point without taking the frame (wave 18).
        assert_eq!(peek_last_deopt_point(), live_addr);
        assert_eq!(peek_last_deopt_point(), live_addr, "a peek takes nothing");
        let (f, cause, addr) = take_last_deopt_with_point().expect("stashed");
        assert_eq!(addr, live_addr);
        assert!(live.owns_deopt_point(addr), "the live body's own point");
        // Take-inspect-restash keeps the address; the cause-only restash drops it.
        restash_last_deopt_with_point(f, cause, addr);
        let (f, cause, again) = take_last_deopt_with_point().expect("restashed");
        assert_eq!(again, live_addr);
        restash_last_deopt_with_cause(f, cause);
        let (_, _, dropped) = take_last_deopt_with_point().expect("restashed");
        assert_eq!(dropped, 0, "an unknown point is 0");
        assert!(!live.owns_deopt_point(0), "0 is never owned");

        assert_eq!(ir_deopt_entry(&retired, 0, std::ptr::null()), i64::MIN);
        let (_, _, addr) = take_last_deopt_with_point().expect("stashed");
        assert_eq!(addr, retired_addr);
        assert!(
            !live.owns_deopt_point(addr),
            "a point of another body is not the live body's"
        );

        // A single-pass artifact's points are the elements of `deopt_points`.
        live.deopt_points
            .push(point_with(ResumeSemantics::REEXECUTE));
        live.deopt_points
            .push(point_with(ResumeSemantics::REEXECUTE));
        let second = &live.deopt_points[1] as *const DeoptimizationPoint as usize;
        assert!(live.owns_deopt_point(second));
        let past_the_end = second + std::mem::size_of::<DeoptimizationPoint>();
        assert!(!live.owns_deopt_point(past_the_end));
    });
}

/// Both trampoline entries route an exceptional frame to the SAME stash.
///
/// `ir_deopt_entry` used to hand a `PendingException` point to
/// `ordinary_stash_frame`, which refuses a non-re-execute point and replaces
/// it with the identity-less re-run sentinel — losing the precise handler
/// locals and charging [`deopt_stash_non_reexecute_refused`], a counter whose
/// own doc reads "non-zero names a producer that broke the stash's
/// invariant". Unreachable today, which is exactly why nothing would notice:
/// no IR-tier producer stamps reason 9 yet (`ir_lower::emit_deopt_unless`
/// says so in a comment).
#[test]
fn an_exceptional_point_reaches_the_exceptional_stash_from_either_entry() {
    on_a_fresh_thread(|| {
        let mut exc = point_with(ResumeSemantics::RETHROW);
        exc.reason = DeoptReason::PendingException;
        assert_eq!(ir_deopt_entry(&exc, 0, std::ptr::null()), i64::MIN);
        assert!(
            take_last_deopt().is_none(),
            "an exceptional frame must not enter the ordinary stash at all"
        );
        let f = take_exceptional_frame().expect("the exceptional stash holds it");
        assert_eq!((f.method_key.as_str(), f.bci), ("P.m:(I)V", 7));
        assert_eq!(
            f.locals,
            vec![FrameValue::Int(5)],
            "and it keeps the precise locals the handler reads"
        );
    });
}

/// Take-inspect-restash is not nesting: the consumer is putting back what
/// it just took.
#[test]
fn a_restash_is_not_counted_as_nesting() {
    on_a_fresh_thread(|| {
        restash_last_deopt(frame("A.m:()V", 1, 0x10));
        let f = take_last_deopt().expect("stashed");
        restash_last_deopt(f);
        // Asserted on the thread-local depth rather than on the
        // process-wide counter, which sibling tests also move.
        LAST_DEOPT
            .with(|c| assert_eq!(c.borrow().len(), 1, "a restash replaces, it does not nest"));
        assert_eq!(peek_last_deopt_identity(), Some(("A.m:()V".to_string(), 1)));
        let _ = take_last_deopt();
    });
}

/// The stack is bounded: past [`MAX_STASH_DEPTH`] the OUTERMOST entry is
/// dropped, because the innermost is the one the next consumer will be
/// asked about.
#[test]
fn the_stash_is_bounded_and_drops_the_outermost() {
    on_a_fresh_thread(|| {
        for i in 0..(MAX_STASH_DEPTH as u32 + 3) {
            LAST_DEOPT.with(|c| push_stash(c, frame("M.m:()V", i, 0x10), false));
        }
        LAST_DEOPT.with(|c| {
            let slot = c.borrow();
            assert_eq!(slot.len(), MAX_STASH_DEPTH);
            assert_eq!(slot[0].frame.bci, 3, "the three oldest were dropped");
            assert_eq!(
                slot[MAX_STASH_DEPTH - 1].frame.bci,
                MAX_STASH_DEPTH as u32 + 2
            );
        });
        let _ = take_last_deopt();
    });
}

/// JIT round 17 wave 6, lane stashcap: the exceptional stash keeps its
/// frames past [`MAX_STASH_DEPTH`] (they may name holds only a VM-side
/// take can release), and a take hands every one of them back as a
/// discard, outermost first.
#[test]
fn the_exceptional_stash_keeps_frames_past_the_ordinary_cap() {
    on_a_fresh_thread(|| {
        let deep = MAX_STASH_DEPTH as u32 + 4;
        for i in 0..deep {
            LAST_EXCEPTIONAL.with(|c| {
                push_stash_with_cause(c, frame("R.down:()V", i, 0x10), None, 0, false)
            });
        }
        assert_eq!(
            exceptional_stash_depth(),
            deep as usize,
            "nothing was dropped"
        );
        let (top, _, _, discarded) =
            take_exceptional_frame_with_point_and_discards().expect("a frame is stashed");
        assert_eq!(top.bci, deep - 1);
        assert_eq!(discarded.len(), deep as usize - 1);
        assert_eq!(
            discarded[0].0.bci, 0,
            "the outermost frame survived the push"
        );
        assert_eq!(exceptional_stash_depth(), 0);
    });
}

/// The null-pointer re-run sentinel is pushed, not assigned: it used to
/// overwrite a real stashed frame with an identity-less one.
#[test]
fn the_null_point_sentinel_does_not_overwrite_a_real_frame() {
    on_a_fresh_thread(|| {
        restash_last_deopt(frame("Callee.b:()V", 3, 0x1000));
        let r = ir_deopt_entry(std::ptr::null(), 0, std::ptr::null());
        assert_eq!(r, i64::MIN);
        LAST_DEOPT.with(|c| {
            let slot = c.borrow();
            assert_eq!(slot.len(), 2);
            assert_eq!(slot[0].frame.method_key, "Callee.b:()V");
            assert_eq!(slot[1].frame.bci, u32::MAX, "the sentinel is on top");
        });
        let _ = take_last_deopt();
    });
}

/// The exceptional stash can now be cleared by its owner only.
#[test]
fn an_exceptional_frame_is_cleared_only_by_the_method_that_published_it() {
    on_a_fresh_thread(|| {
        restash_exceptional_frame(frame("Owner.m:()V", 12, 0x30));
        assert_eq!(
            peek_exceptional_identity(),
            Some(("Owner.m:()V".to_string(), 12))
        );
        assert!(
            !clear_exceptional_frame_of("Someone/Else.m:()V", 12),
            "a frame that is not yours is not yours to drop"
        );
        assert!(!clear_exceptional_frame_of("Owner.m:()V", 13), "wrong bci");
        assert!(
            !clear_exceptional_frame_of("", 12),
            "an empty key never matches"
        );
        assert!(clear_exceptional_frame_of("Owner.m:()V", 12));
        assert_eq!(peek_exceptional_identity(), None);
    });
}

/// Interpreter round i1 wave 11: the exceptional stash hands out the
/// point that published a frame, as `LAST_DEOPT` does, and a
/// take-inspect-restash keeps it (and is not counted as nesting).
#[test]
fn an_exceptional_frame_keeps_its_point_across_take_and_restash() {
    on_a_fresh_thread(|| {
        let cause = DeoptCause {
            reason: DeoptReason::PendingException,
            speculation_id: 9,
        };
        LAST_EXCEPTIONAL.with(|c| {
            push_stash_with_cause(c, frame("Owner.m:()V", 4, 0x40), Some(cause), 0xABC0, false)
        });
        let (f, got_cause, point) =
            take_exceptional_frame_with_point().expect("the exceptional stash holds it");
        assert_eq!(f.method_key, "Owner.m:()V");
        assert_eq!(got_cause, Some(cause));
        assert_eq!(point, 0xABC0);
        restash_exceptional_frame(frame("Other.x:()V", 1, 0x50));
        restash_exceptional_frame_with_point(f, got_cause, point);
        let (again, again_cause, again_point) =
            take_exceptional_frame_with_point().expect("restashed on top");
        assert_eq!(again.method_key, "Owner.m:()V");
        assert_eq!(again_cause, Some(cause));
        assert_eq!(again_point, 0xABC0);
        assert!(
            take_exceptional_frame_with_point().is_none(),
            "the take discarded the frame beneath"
        );
        // A plain restash carries no point.
        restash_exceptional_frame(frame("Plain.p:()V", 2, 0x60));
        let (_, plain_cause, plain_point) =
            take_exceptional_frame_with_point().expect("the plain restash");
        assert_eq!(plain_cause, None);
        assert_eq!(plain_point, 0);
    });
}
