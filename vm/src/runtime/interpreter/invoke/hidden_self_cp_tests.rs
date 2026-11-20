use crate::runtime::interpreter::field_access::owner_in_referencing_namespace;
use crate::vm::SharedVm;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions};
use cratonvm_types::ClassId;

const FILE_NAME: &str = "hsr/P";
const HIDDEN_NAME: &str = "hsr/P/0x1f";
/// The `Methodref hsr/P.priv:()I` of [`class_file`].
const PRIV_REF: u16 = 9;

/// A version-52 class `hsr/P extends java/lang/Object` with one method
/// `private priv()I` (`iconst_1; ireturn`) and a `Methodref` naming it
/// through the class's own `this_class` entry (`#9`).
fn class_file() -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&10u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, FILE_NAME); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1 (this_class)
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "priv"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.extend_from_slice(&[12, 0, 5, 0, 6]); // #8 NameAndType priv:()I
    b.extend_from_slice(&[10, 0, 2, 0, 8]); // #9 Methodref hsr/P.priv:()I
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x02, 0, 5, 0, 6, 0, 1]); // private priv()I
    let code = [0x04, 0xac]; // iconst_1; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 1]); // max_stack 1, max_locals 1
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

fn define(shared: &SharedVm, hidden: bool) -> Option<ClassId> {
    let (name, options) = if hidden {
        (
            HIDDEN_NAME,
            DefineClassOptions {
                override_name: Some(HIDDEN_NAME.to_string()),
                hidden: true,
                skip_verification: true,
                ..Default::default()
            },
        )
    } else {
        (FILE_NAME, DefineClassOptions::default())
    };
    shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(name, &class_file(), ClassLoaderId::Application, options)
        .ok()
}

fn owner(shared: &SharedVm, referencing: ClassId) -> Option<ClassId> {
    let cm = shared.classes.class_manager.read();
    owner_in_referencing_namespace(shared, &cm, referencing, FILE_NAME)
}

fn targets_private(shared: &SharedVm, referencing: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    super::invokevirtual_site_targets_private(&cm, referencing, FILE_NAME, "priv", "()I")
}

/// The owner every member check is judged against, and the private
/// selection door, answer the hidden class for its own name: before the
/// ordinary class exists (the name finds nothing) and after (the name
/// finds that ordinary class). `=0` restores both by-name answers.
#[test]
fn a_hidden_classes_own_name_binds_to_the_hidden_class_before_and_after_a_same_named_load() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(hidden) = define(&shared, true) else {
        eprintln!("skipped: this unit-test VM cannot define a hidden class");
        return;
    };
    let bits = shared.classes.hidden_classes.clone();
    assert_eq!(bits.contains(hidden), Some(true), "precondition: the define set the bit");

    bits.set_self_cp_by_id(true);
    assert_eq!(owner(&shared, hidden), Some(hidden));
    assert!(targets_private(&shared, hidden));
    bits.set_self_cp_by_id(false);
    assert!(!targets_private(&shared, hidden), "=0: the name finds no class yet");

    let Some(plain) = define(&shared, false) else {
        eprintln!("skipped: this unit-test VM cannot define the ordinary class");
        return;
    };
    assert_ne!(plain, hidden);
    assert_eq!(owner(&shared, hidden), Some(plain), "=0: the same-named class captures it");

    bits.set_self_cp_by_id(true);
    assert_eq!(owner(&shared, hidden), Some(hidden), "the ordinary class must not capture it");
    assert!(targets_private(&shared, hidden));
    // The ordinary class's own name is its own, as before.
    assert_eq!(owner(&shared, plain), Some(plain));
}

/// `R21Fz5HiddenOsr1`'s `IllegalAccessError`: once the ordinary class is
/// loaded, the hidden class's `Methodref` to its own private method was
/// judged against the ORDINARY class's `priv` (another nest, so refused).
/// It now resolves against the hidden class's own method; and the
/// `invokespecial` owner spells the constant pool's name, never the
/// hidden class's stored `/0x` name, which no lookup finds.
#[test]
fn a_hidden_classes_call_to_its_own_private_method_is_not_refused_after_a_same_named_load() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (Some(hidden), Some(_plain)) = (define(&shared, true), define(&shared, false)) else {
        eprintln!("skipped: this unit-test VM cannot define the fixture");
        return;
    };
    shared.classes.hidden_classes.set_self_cp_by_id(true);
    let resolved = super::resolve_method_metadata_as(&shared, hidden, PRIV_REF, true);
    assert!(
        resolved.is_ok(),
        "the hidden class's own private method was refused: {:?}",
        resolved.err().map(|e| format!("{e:?}"))
    );
    let owner = super::invokespecial_owner_class_name(
        &shared,
        hidden,
        PRIV_REF,
        &std::sync::Arc::from(FILE_NAME),
        "priv",
        "()I",
    );
    assert_eq!(owner.as_deref().ok(), Some(FILE_NAME));
}

/// The JIT's site owner (`find_cp_class_for_class`, which the compiled
/// site doors in `jit::helpers` ask) answers the hidden class too.
#[test]
fn the_cp_class_lookup_the_jit_site_doors_ask_answers_the_hidden_class() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (Some(hidden), Some(plain)) = (define(&shared, true), define(&shared, false)) else {
        eprintln!("skipped: this unit-test VM cannot define the fixture");
        return;
    };
    shared.classes.hidden_classes.set_self_cp_by_id(true);
    let cm = shared.classes.class_manager.read();
    assert_eq!(cm.find_cp_class_for_class(FILE_NAME, hidden), Some(hidden));
    assert_eq!(cm.find_class_by_name_for_class(FILE_NAME, hidden), Some(plain));
    assert_eq!(cm.find_cp_class_for_class(FILE_NAME, plain), Some(plain));
}

/// JIT round 21 wave 21, items 3-4 of `r21-review-w21-findings-FIXED-20261020.md`:
/// a hidden class stored under a name that does not carry its class-file
/// name (the pre-wave-21 `defineAnonymousClass` host-name mint) is still
/// itself for its OWN name, and is not itself for the host's, at every
/// self door: the shared rule reads the recorded class-file name.
#[test]
fn the_self_doors_read_the_recorded_class_file_name_not_the_stored_spelling() {
    const HOST_MINTED: &str = "hsr/Host/0x9";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(hidden) = shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            HOST_MINTED,
            &class_file(),
            ClassLoaderId::Application,
            DefineClassOptions {
                override_name: Some(HOST_MINTED.to_string()),
                hidden: true,
                skip_verification: true,
                ..Default::default()
            },
        )
        .ok()
    else {
        eprintln!("skipped: this unit-test VM cannot define a hidden class");
        return;
    };
    shared.classes.hidden_classes.set_self_cp_by_id(true);
    let cm = shared.classes.class_manager.read();
    use crate::runtime::interpreter::constants as c;
    assert!(c::hidden_self_reference_in(&cm, hidden, FILE_NAME));
    assert!(!c::hidden_self_reference_in(&cm, hidden, "hsr/Host"), "not the host");
    let class = cm.get_class(hidden).expect("defined");
    assert_eq!(&*class.name, HOST_MINTED);
    assert!(c::is_self_class_reference_in(&cm, class, FILE_NAME));
    assert!(!c::is_self_class_reference_in(&cm, class, "hsr/Host"));
    assert_eq!(owner_in_referencing_namespace(&shared, &cm, hidden, FILE_NAME), Some(hidden));
    assert_eq!(cm.find_cp_class_for_class("hsr/Host", hidden), None);
}

/// JIT round 21 wave 22, lane hidden-resid (item 3 residual of
/// `r21-review-w21-findings-FIXED-20261020.md`): `dispatch_static`'s self test
/// asks the recorded-name rule under `CRATONVM_HIDDEN_SELF_RECORDED_NAME`
/// and keeps the pure suffix predicate only on the `=0` arm. The needles
/// are built at run time so this module's own text never matches.
#[test]
fn dispatch_static_self_test_reads_the_recorded_class_file_name() {
    let src = include_str!("../dispatch_static.rs").replace("\r\n", "\n");
    let start = src.find("let self_class_id = {").expect("the self test");
    let body = &src[start..];
    let body = &body[..body.find("\n    };\n").expect("its end")];
    let gate = body.find(concat!("cm.hidden_self_recorded", "_name()")).expect("the switch");
    let ruled = body.find(concat!("is_self_class_reference", "_in(")).expect("the rule");
    let pure = body.find(concat!("is_self_class_reference", "(")).expect("the =0 arm");
    assert!(gate < ruled && ruled < pure, "switch, then the recorded rule, then the old arm");
}

/// Wave 22, lane hidden-resid (item 3 of `r21-review-w22-findings-FIXED-20261020.md`):
/// the unwinder's store-only constructor test asks the class store's one
/// answer (the recorded class-file name) before the stored-suffix rule.
#[test]
fn the_unwinders_constructor_target_reads_the_store_answer() {
    let src = include_str!("../exception_dispatch.rs").replace("\r\n", "\n");
    let at = src.find("\nfn ctor_pool_entry_may_initialize(").expect("the store-only door");
    let body = &src[at..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    let store = body.find(concat!("store.hidden_self", "_name(class, target)")).expect("the store rule");
    let pure = body.find(concat!("ctor_target_may", "_initialize(")).expect("the suffix rule");
    assert!(body.contains(concat!("store.recorded_self", "_name()")), "under the switch");
    assert!(store < pure);
}

/// Wave 22, item 3: a hidden class whose stored name's suffix does not
/// carry its class-file name (the define-time collision probe's
/// `<override>/0x<n>`) is itself at the interpreter's self doors.
#[test]
fn a_collided_hidden_define_is_itself_at_the_self_doors() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let define = || {
        shared.classes.class_manager.write().define_class_with_options(
            HIDDEN_NAME,
            &class_file(),
            ClassLoaderId::Application,
            DefineClassOptions {
                override_name: Some(HIDDEN_NAME.to_string()),
                hidden: true,
                skip_verification: true,
                ..Default::default()
            },
        )
    };
    let (Ok(_first), Ok(second)) = (define(), define()) else {
        eprintln!("skipped: this unit-test VM cannot define a hidden class");
        return;
    };
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(second).expect("defined");
    assert_ne!(&*class.name, HIDDEN_NAME, "the collision probe renamed it");
    use crate::runtime::interpreter::constants as c;
    assert!(c::is_self_class_reference_in(&cm, class, FILE_NAME));
    assert!(cm.class_store.hidden_self_name(class, FILE_NAME));
}

/// Item 2 of the same page: JVMTI's invoke-at-pc readers resolve the
/// owner a `Methodref` names through the constant-pool rule, as their
/// `gc_and_alloc` / `interpreter` siblings do. The needle is built at run
/// time so this module's own text never matches.
#[test]
fn jvmti_invoke_owner_readers_use_the_constant_pool_rule() {
    let src = include_str!("../jvmti_events.rs");
    let by_name = concat!("find_class_by_name_for", "_class(");
    for owner in ["owner_name, ", "&owner_name, "] {
        let needle = format!("{by_name}{owner}");
        assert!(
            !src.contains(&needle),
            "a by-name owner lookup is back in jvmti_events.rs: {needle}"
        );
    }
    assert_eq!(src.matches(concat!("find_cp_class", "_for_class(")).count(), 3);
}
