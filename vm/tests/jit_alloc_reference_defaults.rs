// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compiled TLAB allocation must write typed null defaults for reference-only
//! classes. A zeroed legacy Value cell is Int(0), not Object(None).
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

#[test]
fn compiled_new_writes_reference_only_default() {
    let resources = format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"));
    let java_home = cratonvm_vm::config::resolve_java_home_public(None)
        .expect("reference-default regression requires a real JDK");

    cratonvm_types::flags::with_process_overrides(
        &[
            ("CRATONVM_TIER_C1_THRESHOLD", Some("100000")),
            ("CRATONVM_TIER_C2_THRESHOLD", Some("600")),
            ("CRATONVM_TIER_C2_MIN_INVOCATIONS", Some("500")),
            ("CRATONVM_C2_ACCEPT", Some("always")),
        ],
        || {
            let mut config = VmConfig::new().with_classpath(vec![resources]);
            config.use_synthetic_jdk = false;
            let mut vm = Vm::new(config.with_java_home(java_home.to_string_lossy().into_owned()));
            let class = "cratonvm/JitReferenceOnlyAlloc";
            let holder = "cratonvm/JitReferenceOnlyAlloc$Holder";
            let descriptor = "()Lcratonvm/JitReferenceOnlyAlloc$Holder;";

            let mut allocated = None;
            for _ in 0..1200 {
                allocated = vm
                    .invoke(class, "allocate", descriptor, &[])
                    .expect("allocation method must return normally");
            }

            let class_id = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(class)
                .expect("allocation class must be loaded");
            let holder_id = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(holder)
                .expect("Holder must be loaded");
            assert!(
                vm.shared
                    .jit
                    .jit_cache
                    .read()
                    .get(class, "allocate", descriptor, class_id)
                    .is_some(),
                "allocate must be compiled before inspecting its TLAB allocation"
            );

            let object = match allocated {
                Some(Value::Object(Some(object))) => object,
                other => panic!("allocate must return Holder, got {other:?}"),
            };
            let field_index = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_class(holder_id)
                .and_then(|class| class.find_own_field("value"))
                .map(|(index, _)| index)
                .expect("Holder.value must be present");
            assert_eq!(
                vm.shared.mem.heap.get_field(object, field_index),
                Value::Object(None),
                "compiled new must leave a typed null, not a raw zero Int(0)"
            );
        },
    );
}
