use super::{
    code_has_new_of, jit_new_target_init_pending, note_osr_unresumable_exit,
    osr_unresumable_exits, OSR_EXIT_FRAMELESS_REPLAYS,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::ClassId;
use std::sync::Arc;

fn clinit() -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from("<clinit>"),
        descriptor: Arc::from("()V"),
        access_flags: MethodAccessFlags::STATIC,
        attributes: vec![],
    }
}

/// Only a `new` opcode counts, and only one whose operand is the index:
/// an `anewarray` of the same class does not, nor does a `0xbb` byte that
/// is an operand of another instruction.
#[test]
fn code_has_new_of_matches_only_a_new_of_that_index() {
    // new #7; dup; invokespecial #8; areturn
    let code = [0xbb, 0x00, 0x07, 0x59, 0xb7, 0x00, 0x08, 0xb0];
    assert!(code_has_new_of(&code, 7));
    assert!(!code_has_new_of(&code, 8));
    // iconst_1; anewarray #7; areturn
    let array = [0x04, 0xbd, 0x00, 0x07, 0xb0];
    assert!(!code_has_new_of(&array, 7));
    // sipush 0xbb00; iconst_4; ireturn -- the `0xbb` is an operand byte.
    let operand = [0x11, 0xbb, 0x00, 0x07, 0xac];
    assert!(!code_has_new_of(&operand, 7));
    assert!(!code_has_new_of(&[], 7));
}

/// `new` owes initialization exactly when a `<clinit>` would still run
/// somewhere in the chain it initializes (JVMS §5.5); a class with none,
/// under initialized or initializer-free supers, owes nothing observable.
#[test]
fn a_new_target_is_init_pending_only_while_a_clinit_would_still_run() {
    let shared = SharedVm::new(VmConfig::default());
    let (plain, holder, child) = {
        let mut cm = shared.classes.class_manager.write();
        let plain = cm
            .try_ensure_synthetic_class("cratonvm/test/NewInitPlain", 0)
            .expect("Compatible mode fabricates");
        let holder = cm
            .try_ensure_synthetic_class("cratonvm/test/NewInitHolder", 0)
            .expect("Compatible mode fabricates");
        let child = cm
            .try_ensure_synthetic_class("cratonvm/test/NewInitChild", 0)
            .expect("Compatible mode fabricates");
        // Cut every chain to exactly what this test states.
        for id in [plain, holder, child] {
            cm.set_superclass(id, None);
            cm.set_interfaces(id, Vec::new());
        }
        cm.set_superclass(child, Some(holder));
        cm.class_store
            .get_mut(holder)
            .expect("fabricated")
            .methods
            .push(clinit());
        (plain, holder, child)
    };
    let cm = shared.classes.class_manager.read();
    assert!(
        !jit_new_target_init_pending(&shared, &cm, plain),
        "no <clinit> anywhere"
    );
    assert!(
        jit_new_target_init_pending(&shared, &cm, holder),
        "its own <clinit>"
    );
    assert!(
        jit_new_target_init_pending(&shared, &cm, child),
        "its superclass's <clinit>"
    );
    cm.get_class(holder).expect("fabricated").init_state.store(
        cratonvm_classloading::CLASS_INIT_INITIALIZED,
        std::sync::atomic::Ordering::Release,
    );
    assert!(
        !jit_new_target_init_pending(&shared, &cm, holder),
        "initialized"
    );
    assert!(
        !jit_new_target_init_pending(&shared, &cm, child),
        "superclass initialized"
    );
    assert!(
        jit_new_target_init_pending(&shared, &cm, ClassId::new(u32::MAX - 7)),
        "an unknown class is answered conservatively"
    );
}

/// Wave 8, L5: initialization links first, so a never-linked class whose
/// link is not trivial is pending too, and the lock-free door links it
/// (no Java involved) and resolves the row. A class that fails its link
/// checks stays pending and is left exactly as it was, for the run-time
/// helper to link on a Java thread and throw.
#[test]
fn a_never_linked_new_target_is_pending_until_linked() {
    use super::jit_new_target_pending_after_link;
    use cratonvm_classloading::{ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = SharedVm::new(VmConfig::default());
    let (good, bad) = {
        let mut cm = shared.classes.class_manager.write();
        let good = cm
            .try_ensure_synthetic_class("cratonvm/test/NewLinkGood", 0)
            .expect("Compatible mode fabricates");
        let bad = cm
            .try_ensure_synthetic_class("cratonvm/test/NewLinkBad", 0)
            .expect("Compatible mode fabricates");
        for (id, flags) in [
            (good, ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER),
            // FINAL + ABSTRACT fails Pass 2 (`verify_class_access_flags`).
            (
                bad,
                ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL | ClassAccessFlags::ABSTRACT,
            ),
        ] {
            cm.set_superclass(id, None);
            cm.set_interfaces(id, Vec::new());
            let class = cm.class_store.get_mut(id).expect("fabricated");
            // A never-linked application class: no bootstrap-trust skip.
            class.loader_id = ClassLoaderId::Application;
            class.state = ClassState::Loaded;
            class.access_flags = flags;
            class.methods.clear();
        }
        (good, bad)
    };
    {
        let cm = shared.classes.class_manager.read();
        assert!(
            jit_new_target_init_pending(&shared, &cm, good),
            "never linked"
        );
        assert!(
            jit_new_target_init_pending(&shared, &cm, bad),
            "never linked"
        );
    }
    assert!(
        !jit_new_target_pending_after_link(&shared, good),
        "linked by the door"
    );
    assert!(
        jit_new_target_pending_after_link(&shared, bad),
        "a failing link is left to the run-time helper"
    );
    let cm = shared.classes.class_manager.read();
    let good_class = cm.get_class(good).expect("fabricated");
    assert_eq!(good_class.state, ClassState::Verified);
    assert_eq!(good_class.initializing_thread, None);
    let bad_class = cm.get_class(bad).expect("fabricated");
    assert_eq!(bad_class.state, ClassState::Loaded, "left as it was");
    assert_eq!(bad_class.initializing_thread, None);
    assert!(!jit_new_target_init_pending(&shared, &cm, good));
    let waiters = shared.classes.class_init_waiters.lock();
    assert!(!waiters.contains_key(&good) && !waiters.contains_key(&bad));
}

/// An unresumable OSR exit is counted and denies OSR for the method, so the
/// same cause cannot replay the body's work again at the next back edge.
#[test]
fn an_unresumable_osr_exit_is_counted_and_denies_osr_for_the_method() {
    let shared = SharedVm::new(VmConfig::default());
    let key = crate::jit::tiered::MethodKey::with_class_id(
        ClassId::new(0),
        "cratonvm/test/OsrReplay",
        "loop",
        "()V",
    );
    assert!(!shared.jit.tiered_manager.is_osr_denied(&key));
    let before = osr_unresumable_exits().0;
    note_osr_unresumable_exit(
        &shared,
        &OSR_EXIT_FRAMELESS_REPLAYS,
        ClassId::new(0),
        "cratonvm/test/OsrReplay",
        "loop",
        "()V",
    );
    assert!(osr_unresumable_exits().0 > before);
    assert!(shared.jit.tiered_manager.is_osr_denied(&key));
}

/// The legacy first-call door in `interpreter.rs::execute` must drain the
/// deopt flag on exit and use it to tell a sentinel from a genuine
/// `Long.MIN_VALUE`: it left the flag set after routing an implicit
/// exception, and a compiled caller later misread a `J` return through it
/// (`L7JitDoorOutcomes`). Since wave 5 it does so by sharing
/// `run_jit_body_raw` with the three `jit_bridge` doors instead of keeping
/// a hand-written copy of the drain, so the pin is now: `execute` enters
/// the body only through `run_jit_body_raw`, drains no signal by hand, and
/// `run_jit_body_raw` retires a stale flag on entry and drains every signal
/// on exit. A text pin, because the door is a 3 000-line function no unit
/// test can enter.
#[test]
fn the_legacy_first_call_door_drains_the_deopt_flag() {
    let src = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let start = src.find("\npub fn execute(").expect("execute exists") + 1;
    let rest = &src[start..];
    // The function ends at the first column-0 closing brace.
    let end = rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3);
    let execute = &rest[..end];
    assert!(
        execute.contains("jit_bridge::run_jit_body_raw("),
        "execute()'s first-call door must run the body through run_jit_body_raw"
    );
    for hand_drain in [
        "crate::jit::helpers::take_jit_deopt_pending()",
        "crate::jit::helpers::take_jit_pending_exception(",
        "crate::jit::helpers::take_jit_pending_npe(",
        "crate::jit::helpers::take_jit_pending_aioobe(",
        "crate::jit::helpers::take_jit_pending_arithmetic(",
        "crate::jit::helpers::take_jit_pending_trap_frames(",
        "cratonvm_jit::deopt::take_last_deopt()",
        ".try_call_with_context(",
    ] {
        assert!(
            !execute.contains(hand_drain),
            "execute() must not drain `{hand_drain}` by hand; run_jit_body_raw owns it"
        );
    }
    let bridge = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    let body = &bridge[bridge
        .find("\npub(in crate::runtime::interpreter) fn run_jit_body_raw(")
        .expect("run_jit_body_raw exists")..];
    let body = &body[..body.find("\n\x7d\n").map_or(body.len(), |e| e + 3)];
    assert!(
        body.contains("let _ = crate::jit::helpers::take_jit_deopt_pending();"),
        "a stale deopt flag must be retired before the body runs"
    );
    assert!(
        body.contains("crate::jit::helpers::take_all_jit_signals(thread)"),
        "every signal, the deopt flag included, must be drained after the body"
    );
    assert!(
        body.contains("let deopt_signaled = sig.deopt;"),
        "i64::MIN must be classified by its signals, not its value"
    );
}

/// Interpreter round i1 wave 25, lane L2: both no-exception sentinel arms
/// (every method-entry door's `run_jit_body_raw`, and `try_osr`) record
/// the exceptional stash's depth BEFORE the body runs and release the
/// compiled locks of their own reason-9 frames above it BEFORE the old
/// unreleasing drop. The behaviour is `deopt_resume`'s
/// `a_pad_exit_frame_above_the_floor_releases_its_compiled_locks` /
/// `an_osr_pad_exit_releases_only_what_the_live_record_does_not_name`; this
/// pins the wiring, in two functions no unit test can enter.
#[test]
fn both_sentinel_arms_release_their_own_pad_exits_locks() {
    let bridge = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    for (start, live) in [
        (
            "\npub(in crate::runtime::interpreter) fn run_jit_body_raw(",
            "None,",
        ),
        (
            "\npub(in crate::runtime::interpreter) fn try_osr(",
            "Some((frame_idx, &osr_entry_holds)),",
        ),
    ] {
        let body = &bridge[bridge.find(start).expect("the door exists")..];
        let body = &body[..body.find("\n\x7d\n").map_or(body.len(), |e| e + 3)];
        let floor = body
            .find("let exc_floor = ")
            .expect("the floor is recorded");
        assert!(
            body[floor..].contains("cratonvm_jit::deopt::exceptional_stash_depth()"),
            "{start}: the floor is the stash depth"
        );
        let enter = body
            .find("set_jit_thread(thread)")
            .expect("the body is entered");
        assert!(
            floor < enter,
            "{start}: the floor must be read before the body runs"
        );
        let release = body
            .find("super::deopt_resume::release_locks_of_own_pad_exits(")
            .expect("the sentinel arm releases the pad exits' locks");
        // JIT round 17 wave 4, lane stashdrop: the drop releases what it
        // drops under the exceptional hand-over's switches, and is the old
        // `drop_own_exceptional_frame` otherwise.
        let old_drop = body
            .find(
                "super::deopt_resume::CompiledLocksOfAStash::\
                 drop_own_exceptional_frame_releasing(",
            )
            .expect("the old drop stays for a frame below the floor");
        assert!(
            enter < release && release < old_drop,
            "{start}: release, then drop"
        );
        assert!(
            body[release..old_drop].contains(live),
            "{start}: the live-frame argument is `{live}`"
        );
    }
}
