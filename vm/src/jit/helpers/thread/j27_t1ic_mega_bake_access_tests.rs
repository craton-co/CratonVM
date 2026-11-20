//! j27 wave 7 (lane T1IC, IC21-2 of `jit-r21-ic-proposals-RETIRED-20261101.md`): the
//! access rule of the compile-time bind (`mega_bake_access_is_certain`),
//! untested since r21 ("they need a ClassManager"). A baked site reads the
//! cells every site of its selector (caller LOADER, not class) publishes,
//! so the bind is allowed only where the caller's resolution of the
//! method cannot fail its access check: owner and caller in one runtime
//! package, an instance non-private method that is public or of that
//! package. Every refusal keeps the first-publication bind.
use super::j27_disprows_dispatch_target_tests::one_method_class;
use super::*;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions};

const CALLER: &str = "j27t1ic/p/Caller";

fn info() -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "j27t1ic/p/Owner",
        method_name: "m",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

#[test]
fn the_bake_is_certain_only_inside_the_callers_runtime_package() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    // (owner name, method flags, expected answer, why)
    let rows: [(&str, u16, bool, &str); 6] = [
        ("j27t1ic/p/Public", 0x0001, true, "public, same package"),
        ("j27t1ic/p/Package", 0x0000, true, "package-private, same package"),
        ("j27t1ic/p/Private", 0x0002, false, "private: never a virtual bind"),
        ("j27t1ic/p/Static", 0x0009, false, "static: resolution throws ICCE"),
        ("j27t1ic/q/Other", 0x0001, false, "owner in another package"),
        ("j27t1ic/p/Forked", 0x0001, false, "same name package, another loader"),
    ];
    let defined = {
        let mut cm = shared.classes.class_manager.write();
        let mut define = |name: &str, flags: u16| {
            cm.define_class_with_options(
                name,
                &one_method_class(name, "java/lang/Object", flags),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            )
            .ok()
        };
        let caller = define(CALLER, 0x0001);
        let owners: Vec<Option<ClassId>> =
            rows.iter().map(|&(name, flags, _, _)| define(name, flags)).collect();
        // The class manager's test side-door: the last owner moves to a
        // user loader under its own name (another runtime package).
        if let Some(Some(forked)) = owners.last() {
            if let Some(class) = cm.class_store.get_mut(*forked) {
                class.loader_id = ClassLoaderId::UserDefined(27_071);
            }
        }
        caller.zip(owners.into_iter().collect::<Option<Vec<ClassId>>>())
    };
    // j27 TRIC: a silent return here made the test pass without one
    // assert; the default test VM defines the classes, so refusing to
    // define them is a failure, not a skip.
    let (caller, owners) = defined.expect("the test VM defines the caller and owner classes");
    let cm = shared.classes.class_manager.read();
    for (&(name, _, expected, why), &owner) in rows.iter().zip(owners.iter()) {
        assert_eq!(
            mega_bake_access_is_certain(&cm, caller, owner, &info()),
            expected,
            "{name}: {why}"
        );
    }
    // A method the owner's chain does not declare: nothing to bake.
    let absent = JitInvokeInfo {
        method_name: "absent",
        ..info()
    };
    assert!(
        !mega_bake_access_is_certain(&cm, caller, caller, &absent),
        "an unresolvable method refuses"
    );
}

// j27 wave 7 (lane T1IC): the helper switches were process `OnceLock`
// latches, which kept whichever answer the first reader in the test
// binary saw. As `MemoSlot`s they follow a thread override both ways.
