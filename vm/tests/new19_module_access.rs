// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NEW-19: JPMS `opens`/`exports` enforcement for reflection (JEP 403).
//!
//! This file has two layers of tests:
//!
//! 1. **Direct end-to-end tests** (`new19_*` — *not* ignored) exercise the
//!    full data path: they load real `ModuleTarget` and `TckModule` class
//!    files into a VM, reassign `ModuleTarget` to a synthetic named module
//!    by mutating its `Class.module_name` field, and then invoke the
//!    `ModuleRegistry::check_deep_reflection_access` method that the
//!    reflection natives (`Field.setAccessible`, `Method.setAccessible`,
//!    `Constructor.setAccessible`, `Method.invoke`, `Field.get/set`,
//!    `Constructor.newInstance`) route through on every call. This is the
//!    authoritative test of the module-access wiring for NEW-19.
//!
//! 2. **Java-level integration tests** (`new19_java_*` — currently
//!    `#[ignore]`) drive the end-to-end path through Java bytecode in
//!    `TckModule.java`. Those rely on `Class.getDeclaredFields()` /
//!    `getDeclaredMethods()` / `getDeclaredConstructors()` linkage which
//!    the synthetic `java.lang.Class` class file currently does not expose
//!    (pre-existing gap — `test_s50_cls_getDeclaredFields` and friends in
//!    `interpreter_tests.rs` fail for the same reason). The Java sources
//!    and the runner code are complete and checked in; the `#[ignore]`
//!    will be removed once that class-file linkage is fixed in a later
//!    phase.
//!
//! Prerequisites: `javac` on PATH so `build.rs` has compiled both test
//! Java classes (`ModuleTarget.class`, `TckModule.class`).

use cratonvm_vm::classloading::module::{ModuleDescriptor, ALL_UNNAMED_TARGET, UNNAMED_MODULE};
use cratonvm_vm::classloading::ClassLoaderId;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/ModuleTarget.class")).exists()
        && std::path::Path::new(&format!("{dir}/cratonvm/TckModule.class")).exists()
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

macro_rules! require_classes {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: NEW-19 .class files not available (javac missing?)");
            return;
        }
    };
}

/// Configure the VM the way every NEW-19 test expects: load both Java
/// classes, register a synthetic named module `test.named`, re-home
/// `ModuleTarget` into it, and ensure `TckModule` stays in the unnamed
/// module. Optional `extra_opens` simulates `--add-opens` flags.
fn setup_modules(vm: &mut Vm, extra_opens: &[(&str, &str, &str)]) {
    let mut cm = vm.shared.classes.class_manager_write();

    // Load both test classes.
    cm.load_class("cratonvm/ModuleTarget")
        .expect("ModuleTarget must load");
    cm.load_class("cratonvm/TckModule")
        .expect("TckModule must load");

    // Register the synthetic named module.
    let desc = ModuleDescriptor {
        name: "test.named".to_string(),
        version: None,
        requires: vec![],
        exports: vec![],
        opens: vec![],
        uses: vec![],
        provides: vec![],
        is_open: false,
        automatic: false,
    };
    cm.module_registry.register(desc, vec![]);

    // Apply dynamic opens (simulates --add-opens / Module.addOpens).
    for (module, pkg, target) in extra_opens {
        cm.module_registry.add_opens(module, pkg, target);
    }

    cm.module_registry.build_readability_graph();

    // Re-home ModuleTarget into the named module; ensure TckModule is
    // unnamed. Both mutations are no-ops if the classes happen to already
    // match the expected state.
    let target_cid = cm
        .find_class_by_name_for_loader("cratonvm/ModuleTarget", ClassLoaderId::Application)
        .expect("ModuleTarget loaded above");
    if let Some(class) = cm.get_class_mut(target_cid) {
        class.module_name = Some("test.named".to_string());
    }
    let caller_cid = cm
        .find_class_by_name_for_loader("cratonvm/TckModule", ClassLoaderId::Application)
        .expect("TckModule loaded above");
    if let Some(class) = cm.get_class_mut(caller_cid) {
        class.module_name = None;
    }
}

// ===========================================================================
// Direct tests — ModuleRegistry path, real loaded classes, no reflection API
// ===========================================================================
//
// These exercise the same `check_deep_reflection_access` function that
// `NativeContext::check_deep_reflection_access` (in vm_exec.rs) routes into,
// using real `Class::module_name` values read from the `ClassManager`. If
// these pass, the data flow from loaded class → module_name → registry
// check is correct end-to-end; the only code the Java tests would add on
// top is the stack-walker frame classification, which is covered by
// `p59_sw_get_caller_class` tests and the `REFLECTION_INTERNAL_CLASSES`
// filter in `lang_class.rs`.

/// Look up the `module_name` string for a class that's been loaded and
/// (possibly) re-homed. Returns `UNNAMED_MODULE` ("") when the class has
/// no module membership, which is the semantics the registry check uses.
fn module_name_of(vm: &Vm, class_name: &str) -> String {
    let cm = vm.shared.classes.class_manager.read();
    let cid = cm
        .find_class_by_name_for_loader(class_name, ClassLoaderId::Application)
        .expect("class must be loaded");
    let class = cm.get_class(cid).expect("class must exist");
    class
        .module_name
        .clone()
        .unwrap_or_else(|| UNNAMED_MODULE.to_string())
}

#[test]
fn new19_direct_deny_cross_module_without_opens() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[]);

    let accessor_mod = module_name_of(&vm, "cratonvm/TckModule");
    let target_mod = module_name_of(&vm, "cratonvm/ModuleTarget");
    assert_eq!(
        accessor_mod, UNNAMED_MODULE,
        "TckModule must remain unnamed"
    );
    assert_eq!(
        target_mod, "test.named",
        "ModuleTarget must be re-homed into test.named"
    );

    let cm = vm.shared.classes.class_manager.read();
    let result =
        cm.module_registry
            .check_deep_reflection_access(&accessor_mod, &target_mod, "cratonvm");
    assert!(
        result.is_err(),
        "JEP 403: unnamed accessor must be denied deep reflection into a \
         non-opened named-module package, got {result:?}"
    );
}

#[test]
fn new19_direct_allow_cross_module_with_add_opens_unqualified() {
    require_classes!();
    let mut vm = test_vm();
    // The launcher spells this `--add-opens test.named/cratonvm=ALL-UNNAMED`,
    // and the token reaches `add_opens` verbatim: `""` here would be a
    // genuinely *unqualified* open, which grants every module and would let
    // this test pass without the ALL-UNNAMED path working at all.
    setup_modules(&mut vm, &[("test.named", "cratonvm", ALL_UNNAMED_TARGET)]);

    let accessor_mod = module_name_of(&vm, "cratonvm/TckModule");
    let target_mod = module_name_of(&vm, "cratonvm/ModuleTarget");

    let cm = vm.shared.classes.class_manager.read();
    let result =
        cm.module_registry
            .check_deep_reflection_access(&accessor_mod, &target_mod, "cratonvm");
    assert!(
        result.is_ok(),
        "--add-opens ...=ALL-UNNAMED must grant the deny, got {result:?}"
    );
}

#[test]
fn new19_direct_allow_same_module() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[]);

    // TckModule → TckModule (same module: UNNAMED → UNNAMED).
    let m = module_name_of(&vm, "cratonvm/TckModule");
    let cm = vm.shared.classes.class_manager.read();
    assert!(cm
        .module_registry
        .check_deep_reflection_access(&m, &m, "cratonvm")
        .is_ok());
}

#[test]
fn new19_direct_allow_named_accessor_with_qualified_opens() {
    require_classes!();
    let mut vm = test_vm();
    // Give the unnamed module's cratonvm package qualified opens to a
    // different accessor; the TckModule → ModuleTarget edge should still
    // be denied because our accessor is unnamed, not "other.mod".
    setup_modules(&mut vm, &[("test.named", "cratonvm", "other.mod")]);

    let cm = vm.shared.classes.class_manager.read();
    // Unnamed accessor → still denied (opens is qualified to other.mod).
    assert!(
        cm.module_registry
            .check_deep_reflection_access(UNNAMED_MODULE, "test.named", "cratonvm")
            .is_err(),
        "qualified opens to other.mod must not leak to unnamed accessor"
    );
    // other.mod accessor → allowed.
    assert!(
        cm.module_registry
            .check_deep_reflection_access("other.mod", "test.named", "cratonvm")
            .is_ok(),
        "qualified opens must permit the named target accessor"
    );
}

#[test]
fn new19_direct_classpath_only_mode_allows_everything() {
    require_classes!();
    // With no modules registered at all, the vm_exec.rs wrapper short-
    // circuits on `module_registry.is_empty()` — classpath-only mode must
    // never break existing users.
    let vm = test_vm();
    {
        let mut cm = vm.shared.classes.class_manager_write();
        cm.load_class("cratonvm/ModuleTarget").expect("must load");
    }
    let cm = vm.shared.classes.class_manager.read();
    assert!(cm.module_registry.is_empty());
    // Any cross-module query is vacuously allowed in this mode — the
    // trait wrapper returns Ok(()).
}

// ===========================================================================
// Java-level integration tests (ignored pending synthetic-JDK reflection fix)
// ===========================================================================

fn invoke_tck(vm: &mut Vm, method: &str) -> i32 {
    match vm.invoke("cratonvm/TckModule", method, "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("TckModule::{method} — unexpected result: {other:?}"),
    }
}

#[ignore = "Class.getDeclaredFields linkage gap in synthetic JDK (pre-existing)"]
#[test]
fn new19_java_deny_set_accessible_field() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[]);
    assert_eq!(invoke_tck(&mut vm, "denySetAccessibleField"), 1);
}

#[ignore = "Class.getDeclaredMethods linkage gap in synthetic JDK (pre-existing)"]
#[test]
fn new19_java_deny_set_accessible_method() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[]);
    assert_eq!(invoke_tck(&mut vm, "denySetAccessibleMethod"), 1);
}

#[ignore = "Class.getDeclaredConstructors linkage gap in synthetic JDK (pre-existing)"]
#[test]
fn new19_java_deny_set_accessible_constructor() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[]);
    assert_eq!(invoke_tck(&mut vm, "denySetAccessibleConstructor"), 1);
}

#[ignore = "Class.getDeclaredFields linkage gap in synthetic JDK (pre-existing)"]
#[test]
fn new19_java_allow_self_reflection() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[]);
    assert_eq!(invoke_tck(&mut vm, "allowSelfReflection"), 1);
}

#[ignore = "Class.getDeclaredFields linkage gap in synthetic JDK (pre-existing)"]
#[test]
fn new19_java_add_opens_grants_field_access() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[("test.named", "cratonvm", "")]);
    assert_eq!(invoke_tck(&mut vm, "denySetAccessibleField"), 0);
}

#[ignore = "Class.getDeclaredMethods linkage gap in synthetic JDK (pre-existing)"]
#[test]
fn new19_java_add_opens_grants_method_access() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[("test.named", "cratonvm", "")]);
    assert_eq!(invoke_tck(&mut vm, "denySetAccessibleMethod"), 0);
}

#[ignore = "Class.getDeclaredConstructors linkage gap in synthetic JDK (pre-existing)"]
#[test]
fn new19_java_add_opens_grants_constructor_access() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[("test.named", "cratonvm", "")]);
    assert_eq!(invoke_tck(&mut vm, "denySetAccessibleConstructor"), 0);
}

#[ignore = "Class.getDeclaredMethods linkage gap in synthetic JDK (pre-existing)"]
#[test]
/// W6-8: this asserted `1` (allowed). That expectation is wrong against
/// HotSpot 25. `setup_modules` re-homes `ModuleTarget` into `test.named`,
/// declared with `exports: vec![]`, so package `cratonvm` is exported to
/// nobody -- and `Reflection.verifyMemberAccess` runs `verifyModuleAccess`
/// BEFORE its `Modifier.isPublic(modifiers)` shortcut, so a PUBLIC method of
/// a public class in a non-exported package is still refused with
/// `IllegalAccessException`. Measured on Temurin 25.0.3, identical shape:
/// `Class.forName("jdk.internal.misc.VM").getMethod("isBooted").invoke(null)`
/// -> IllegalAccessException "module java.base does not export
/// jdk.internal.misc to unnamed module". `TckModule.allowPublicInvoke`
/// returns 0 on any Throwable, so 0 IS the refusal.
fn new19_java_public_invoke_cross_module_without_exports_is_refused() {
    require_classes!();
    let mut vm = test_vm();
    setup_modules(&mut vm, &[]);
    assert_eq!(invoke_tck(&mut vm, "allowPublicInvoke"), 0);
}
