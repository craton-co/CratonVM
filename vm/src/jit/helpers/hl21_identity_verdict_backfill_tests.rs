//! JIT round 21, lane hashlock, wave 3 (tag `hl21`, HASHLOCK21-4): the
//! identity-hash / equals verdict bytes are filed for classes that are
//! initialised before a table exists (the backfill at the table's first
//! ask) and after it (the `<clinit>` hook), and the backfill is bounded.
use super::*;
use crate::config::VmConfig;
use crate::vm::realms::jit_realm::IdentityHashClassBytes;
use cratonvm_jit::ir_lower as jl;

const EARLY: &str = "cratonvm/test/hl21/Early";
const LATE: &str = "cratonvm/test/hl21/Late";
const IDLE: &str = "cratonvm/test/hl21/NeverInitialised";

fn ask(shared: &SharedVm, tag: i64) -> Option<i64> {
    vh_const_well_known_answer(shared, jl::vh_const_query(tag, 0, 0))
}

/// `None` where the environment's policy refuses a synthetic class
/// (`--real-jdk`): the tests then have nothing to say.
fn define(shared: &SharedVm, name: &str) -> Option<ClassId> {
    let mut cm = shared.classes.class_manager.write();
    cm.try_ensure_synthetic_class(name, 0).ok()
}

fn mark_initialised(shared: &SharedVm, id: ClassId) {
    let bits = &shared.classes.class_init_bits;
    bits.set_as_of(id.as_u32(), bits.reset_generation());
    assert!(bits.is_set(id.as_u32()));
}

/// The byte the verdict function's answer files: `PROVEN` for `Some(true)`,
/// `REFUSED` for `Some(false)`, still `UNKNOWN` for `None`.
fn filed(verdict: Option<bool>) -> u8 {
    match verdict {
        Some(true) => jl::IDENTITY_HASH_CLASS_PROVEN,
        Some(false) => jl::IDENTITY_HASH_CLASS_REFUSED,
        None => jl::IDENTITY_HASH_CLASS_UNKNOWN,
    }
}

/// A class initialised BEFORE the table exists is filed by the backfill at
/// the table's first ask; one never initialised is left unknown.
#[test]
fn hl21_the_first_table_ask_backfills_the_initialised_classes() {
    let shared = SharedVm::new(VmConfig::default());
    let (Some(early), Some(idle)) = (define(&shared, EARLY), define(&shared, IDLE)) else {
        return;
    };
    mark_initialised(&shared, early);
    assert!(!shared.jit.identity_hash_classes.is_allocated());
    // The clinit hook is a no-op before a table exists.
    note_class_initialized_identity_verdicts(&shared, early);
    assert!(!shared.jit.identity_hash_classes.is_allocated());

    let addr = ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG).unwrap();
    assert_ne!(addr, 0);
    let table = &shared.jit.identity_hash_classes;
    assert_eq!(
        table.verdict(early.as_u32()),
        filed(class_hash_code_identity_verdict(&shared, early)),
        "an initialised class is filed by the backfill"
    );
    assert_ne!(
        table.verdict(early.as_u32()),
        jl::IDENTITY_HASH_CLASS_UNKNOWN
    );
    assert_eq!(
        table.verdict(idle.as_u32()),
        jl::IDENTITY_HASH_CLASS_UNKNOWN,
        "an uninitialised class is not"
    );
    // The equals table opens with it (switch on), and is backfilled too.
    let equals = &shared.jit.identity_equals_classes;
    if equals.is_allocated() {
        assert_eq!(
            equals.verdict(early.as_u32()),
            filed(class_equals_identity_verdict(&shared, early))
        );
        assert_ne!(
            equals.verdict(early.as_u32()),
            jl::IDENTITY_HASH_CLASS_UNKNOWN
        );
        assert_eq!(
            equals.verdict(idle.as_u32()),
            jl::IDENTITY_HASH_CLASS_UNKNOWN
        );
    }
}

/// With a table open, the hook files the verdicts of a class the moment
/// its initialisation completes; a second call never rewrites them.
#[test]
fn hl21_the_clinit_hook_files_verdicts_once_a_table_exists() {
    let shared = SharedVm::new(VmConfig::default());
    let Some(late) = define(&shared, LATE) else {
        return;
    };
    let _ = ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG);
    let table = &shared.jit.identity_hash_classes;
    assert_eq!(
        table.verdict(late.as_u32()),
        jl::IDENTITY_HASH_CLASS_UNKNOWN,
        "defined after the table: not filed yet"
    );
    mark_initialised(&shared, late);
    note_class_initialized_identity_verdicts(&shared, late);
    let want = filed(class_hash_code_identity_verdict(&shared, late));
    assert_ne!(want, jl::IDENTITY_HASH_CLASS_UNKNOWN);
    assert_eq!(table.verdict(late.as_u32()), want);
    let equals = &shared.jit.identity_equals_classes;
    if equals.is_allocated() {
        assert_eq!(
            equals.verdict(late.as_u32()),
            filed(class_equals_identity_verdict(&shared, late))
        );
    }
    // Write-once: a later call (or a hand-written other value) keeps it.
    table.record(late.as_u32(), jl::IDENTITY_HASH_CLASS_PROVEN + 7);
    note_class_initialized_identity_verdicts(&shared, late);
    assert_eq!(table.verdict(late.as_u32()), want);
    // `java/lang/Object` (id 0) is never filed: the arm's exact compare
    // answers it before the table is read.
    note_class_initialized_identity_verdicts(&shared, ClassId::new(0));
    assert_eq!(table.verdict(0), jl::IDENTITY_HASH_CLASS_UNKNOWN);
}

/// The backfill is bounded: a table admits only a few passes, and none
/// after one saw every class.
#[test]
fn hl21_the_backfill_is_bounded_and_ends_when_complete() {
    let t = IdentityHashClassBytes::new();
    let passes = (0..32).filter(|_| t.begin_backfill()).count();
    assert_eq!(passes, IdentityHashClassBytes::BACKFILL_TRIES as usize);
    let done = IdentityHashClassBytes::new();
    assert!(done.begin_backfill());
    done.finish_backfill();
    assert!(!done.begin_backfill());
    assert!(!done.begin_backfill());
}
