//! zt t18/df7 (x13 review items 21b, 21c, 21d on
//! `docs/feature-designs/zgc-architecture.md`): the
//! unbound define's owner thread, the unwind withdraw, and the second
//! definer's wait-then-define.
use super::*;
use crate::test_utils::{
    mock_heap_is_zgc, mock_unbound_define, mock_unbound_defines_reset, mock_withdrawn_defines,
    MockNativeContext,
};
use cratonvm_native_api::NativeClassAccess;
use cratonvm_types::ClassId;
use std::cell::Cell;

thread_local! {
    /// `Thread.yield()` calls the hook saw, the round on which it settles
    /// `SETTLE_ID`, and how (1: withdraw, 2: bind).
    static YIELDS: Cell<u32> = const { Cell::new(0) };
    static SETTLE_AT: Cell<u32> = const { Cell::new(0) };
    static SETTLE_HOW: Cell<u8> = const { Cell::new(0) };
    static SETTLE_ID: Cell<u32> = const { Cell::new(0) };
}

/// Counts `Thread.yield()`; on round `SETTLE_AT` settles `SETTLE_ID` as
/// the other definer's door would.
fn yield_hook(
    ctx: &mut MockNativeContext,
    class_name: &str,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if class_name != "java/lang/Thread" || method_name != "yield" {
        return None;
    }
    let round = YIELDS.with(|y| {
        y.set(y.get() + 1);
        y.get()
    });
    if round == SETTLE_AT.with(Cell::get) {
        let id = ClassId::new(SETTLE_ID.with(Cell::get));
        match SETTLE_HOW.with(Cell::get) {
            1 => {
                ctx.withdraw_unbound_define(id);
            }
            2 => {
                ctx.bind_unbound_define(id);
            }
            _ => {}
        }
    }
    Some(Ok(None))
}

fn arm(at: u32, how: u8, id: u32) {
    YIELDS.with(|y| y.set(0));
    SETTLE_AT.with(|c| c.set(at));
    SETTLE_HOW.with(|c| c.set(how));
    SETTLE_ID.with(|c| c.set(id));
}

/// 21c / 21b: inside its door the unbound define is this thread's own,
/// and only there; a door that did not ask marks nothing; a panic before
/// the settle withdraws the row and resumes with its own payload; a panic
/// after the settle withdraws nothing.
#[test]
fn zt18df7_the_door_owns_its_unbound_define_and_an_unwind_withdraws_it() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x18df_7001);
    mock_unbound_defines_reset();
    let held = ClassId::new(51);
    mock_unbound_define(51, "zt18df7/Held", 7);
    assert!(!unbound_define_held_here(&ctx, held));
    let inside = hold_unbound_define_across(&mut ctx, held, true, |c| {
        unbound_define_held_here(&*c, held)
    });
    assert!(inside, "the door's own define");
    assert!(
        !unbound_define_held_here(&ctx, held),
        "released after the door"
    );
    let inside = hold_unbound_define_across(&mut ctx, held, false, |c| {
        unbound_define_held_here(&*c, held)
    });
    assert!(!inside, "not requested: nothing marked");
    // Another VM's define with the same id is not this door's.
    let other_vm = MockNativeContext::new();
    other_vm.set_vm_identity(0x18df_7002);
    let seen_elsewhere = hold_unbound_define_across(&mut ctx, held, true, |_| {
        unbound_define_held_here(&other_vm, held)
    });
    assert!(!seen_elsewhere, "keyed by the VM too");

    // 21b: a panic between the define and the settle.
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hold_unbound_define_across(&mut ctx, held, true, |_| -> () {
            panic!("zt18df7 door panic")
        })
    }));
    let payload = unwound.expect_err("the panic is resumed");
    assert_eq!(
        payload.downcast_ref::<&str>().copied(),
        Some("zt18df7 door panic"),
        "with its own payload"
    );
    assert_eq!(mock_withdrawn_defines(), 1, "the row is withdrawn");
    assert_eq!(
        ctx.unbound_define_of("zt18df7/Held", 7),
        None,
        "the name is free"
    );
    assert!(!unbound_define_held_here(&ctx, held));

    // A panic after the settle: the row is gone, nothing to withdraw.
    let settled = ClassId::new(52);
    mock_unbound_define(52, "zt18df7/Settled", 7);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hold_unbound_define_across(&mut ctx, settled, true, |c| -> () {
            assert_eq!(c.bind_unbound_define(settled), Some(true));
            panic!("zt18df7 panic after the settle")
        })
    }));
    assert!(unwound.is_err());
    assert_eq!(mock_withdrawn_defines(), 1, "bound: nothing withdrawn");
    assert_eq!(
        ctx.class_id_defined_by_loader_exact("zt18df7/Settled", 7),
        Some(settled)
    );
    mock_unbound_defines_reset();
}

/// 21c: a define of a name whose unbound define is this thread's own does
/// not wait (no `Thread.yield()`); another thread's is waited for until it
/// settles (D6).
#[test]
fn zt18df7_a_define_never_waits_on_its_own_unbound_define() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x18df_7003);
    ctx.set_invoke_hook(yield_hook);
    mock_heap_is_zgc(true);
    mock_unbound_defines_reset();
    let own = ClassId::new(53);
    mock_unbound_define(53, "zt18df7/Own", 7);
    arm(0, 0, 0);
    let waited = hold_unbound_define_across(&mut ctx, own, true, |c| {
        wait_for_unbound_define(c, "zt18df7/Own", &[], 7)
    });
    assert!(waited.is_ok());
    assert_eq!(YIELDS.with(Cell::get), 0, "its own define: no wait");

    // Seen from outside its door, the row is another definer's: waited
    // for until it settles (here: withdrawn on the third round).
    arm(3, 1, 53);
    assert!(wait_for_unbound_define(&mut ctx, "zt18df7/Own", &[], 7).is_ok());
    assert_eq!(YIELDS.with(Cell::get), 3, "waited until it settled");
    assert_eq!(ctx.unbound_define_of("zt18df7/Own", 7), None);
    mock_unbound_defines_reset();
    mock_heap_is_zgc(false);
}


/// The three `defineClass0/1/2` doors: the retry options drop the
/// pre-pass parse, the retry sits between the first define and the
/// match, and the success arm runs inside the owner mark.
#[test]
fn zt18df7_the_doors_retry_after_a_withdrawn_rival_and_hold_their_define() {
    let lang_system = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs")).replace("\r\n", "\n");
    for (start, end) in [
        (
            "pub fn native_classloader_define_class1(",
            "pub fn native_classloader_define_class2(",
        ),
        (
            "pub fn native_classloader_define_class2(",
            "pub fn native_classloader_define_class0(",
        ),
        (
            "pub fn native_classloader_define_class0(",
            "pub fn native_perf_attach(",
        ),
    ] {
        let from = lang_system.find(start).expect("start");
        let src = &lang_system[from..from + lang_system[from..].find(end).expect("end")];
        let keep = src
            .find("let retry_opts = bind_late.then(")
            .expect("retry options");
        let no_parse = src[keep..].find("preparsed: None,").expect("no parse") + keep;
        let define = src.find("ctx.define_class_full(").expect("define");
        let retry = src
            .find("redefine_after_unbound_define_settles(")
            .expect("retry");
        let matched = src.find("match defined {").expect("match");
        let hold = src.find("hold_unbound_define_across(").expect("owner mark");
        let add = src.find("add_defined_class_to_loader(").expect("upcall");
        assert!(
            keep < no_parse && no_parse < define,
            "{start}: options first"
        );
        assert!(
            define < retry && retry < matched,
            "{start}: retry before the match"
        );
        assert!(
            matched < hold && hold < add,
            "{start}: the upcall runs held"
        );
    }
}

/// zt t30/dc: each `defineClass0/1/2` door times the rest of
/// `native-mirror` -- the `addClass` upcall, the settle, then the orphan
/// note and trace -- each under its own census row, in that order and
/// before the door's `native-mirror` clock stops; the wait and the
/// retry-options clone have rows of their own.
#[test]
fn zt30dc_doors_name_every_late_binding_step_in_the_census() {
    let lang_system = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs")).replace("\r\n", "\n");
    for (start, end) in [
        (
            "pub fn native_classloader_define_class1(",
            "pub fn native_classloader_define_class2(",
        ),
        (
            "pub fn native_classloader_define_class2(",
            "pub fn native_classloader_define_class0(",
        ),
        (
            "pub fn native_classloader_define_class0(",
            "pub fn native_perf_attach(",
        ),
    ] {
        let from = lang_system.find(start).expect("start");
        let src = &lang_system[from..from + lang_system[from..].find(end).expect("end")];
        let at = |needle: &str| {
            src.find(needle)
                .unwrap_or_else(|| panic!("{start}: {needle}"))
        };
        let wait = at("wait_for_unbound_define(");
        let wait_row = at("DefinePhase::BindWait,");
        let keep = at("let retry_opts = bind_late.then(");
        let keep_row = at("DefinePhase::BindRetryClone,");
        let add = at("add_defined_class_to_loader(");
        let add_row = at("DefinePhase::MirrorAddClass,");
        let settle = at("settle_unbound_define(");
        let settle_row = at("DefinePhase::MirrorBindSettle,");
        let note = at("note_add_class_orphan_on_failure(");
        let trace = at("trace_add_class_door(");
        let tail_row = at("DefinePhase::MirrorAddTail,");
        let mirror_row = at("DefinePhase::NativeMirror,");
        assert!(wait < wait_row && wait_row < keep, "{start}: the wait row");
        assert!(keep < keep_row, "{start}: the clone row");
        assert!(add < add_row && add_row < settle, "{start}: the upcall row");
        assert!(
            settle < settle_row && settle_row < note,
            "{start}: the settle row"
        );
        assert!(
            note < trace && trace < tail_row && tail_row < mirror_row,
            "{start}: the tail row, inside native-mirror"
        );
    }
    // The bind's own rows: the index in the settle, the TLS work in the hold.
    let own = concat!(include_str!("../classloader/mod.rs"), "\n", include_str!("../classloader/closed_url_classloader_ids.rs"), "\n", include_str!("../classloader/load_class_override_memo_key.rs"), "\n", include_str!("../classloader/adopt_add_class_orphan_by_id.rs"), "\n", include_str!("../classloader/builtin_classloader_find_resource_as.rs"), "\n", include_str!("../classloader/ucl_try_define_local_class.rs")).replace("\r\n", "\n");
    let settle = &own[own.find("pub fn settle_unbound_define(").unwrap()..];
    let settle = &settle[..settle.find("\n}\n").unwrap()];
    assert!(settle.contains("DefinePhase::BindIndex"));
    let hold = &own[own
        .find("pub fn hold_unbound_define_across<T>(")
        .unwrap()..];
    let hold = &hold[..hold.find("\n}\n").unwrap()];
    assert!(hold.contains("DefinePhase::BindHold"));
}
