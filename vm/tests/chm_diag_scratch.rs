// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Scratch diagnostic harness for the CHM in-process get-miss investigation
//! (docs/known-issues/vm/chm-get-misses-stored-key-in-process-20260803.md).
//! Deleted before the fix lands.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn test_classpath() -> Vec<String> {
    let mut cp = Vec::new();
    if let Some(compiled) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        cp.push(compiled.to_string());
    }
    cp.push(test_resources_dir());
    cp
}

fn test_vm() -> Vm {
    Vm::new(VmConfig::new().with_classpath(test_classpath()))
}

#[test]
#[ignore = "scratch diagnostic"]
fn chm_diag() {
    let mut vm = test_vm();
    for m in ["returnsOne", "returnsOneThrows", "diag", "preResizeTraced"] {
        let result = vm.invoke("cratonvm/ChmDiagProbe", m, "()I", &[]);
        println!("== {m} => {result:?}");
    }
}

#[test]
#[ignore = "scratch diagnostic"]
fn chm_registry() {
    let vm = test_vm();
    let c = "java/util/concurrent/ConcurrentHashMap";
    for (name, desc) in [
        ("<init>", "()V"),
        ("put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        ("containsKey", "(Ljava/lang/Object;)Z"),
        ("size", "()I"),
        ("keySet", "()Ljava/util/Set;"),
    ] {
        let found = vm
            .shared
            .natives
            .native_methods
            .find(c, name, desc)
            .is_some();
        println!("registry {c}.{name}{desc} -> {found}");
    }
    for (hc, hn, hd) in [
        ("java/util/HashMap", "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        ("java/util/HashMap", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
    ] {
        let found = vm.shared.natives.native_methods.find(hc, hn, hd).is_some();
        println!("registry {hc}.{hn}{hd} -> {found}");
    }
    let cm = vm.shared.classes.class_manager.read();
    for name in ["java/lang/String", "java/util/HashMap"] {
        match cm.get_loaded_class_id(name) {
            Some(id) => {
                let cls = cm.get_class(id).unwrap();
                println!(
                    "class {name} id={id:?} synthetic_stub={} total_fields={} fields={:?}",
                    cls.is_synthetic_stub,
                    cls.num_total_fields,
                    cls.fields
                        .iter()
                        .filter(|f| !f.is_static())
                        .map(|f| (f.name.to_string(), f.descriptor.to_string()))
                        .collect::<Vec<_>>()
                );
            }
            None => println!("class {name} NOT LOADED"),
        }
    }
}
