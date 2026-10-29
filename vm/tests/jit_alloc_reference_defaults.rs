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
            // JIT round 22 wave 7 (lane memory): the C2 task runs on the
            // background compiler thread, so "compiled after exactly 1200
            // calls" was a race against that thread -- one the debug build
            // lost on the w6 tree, whose optimizing pipeline does more work
            // per compile (the test passed on dev in 0.75 s and failed on w6
            // in 1.44 s). Keep calling until the body is published, with a
            // bounded budget, as `jit_bridge_sink_rerun_off_arm.rs` and
            // `cha_unique_implementor_binds.rs` do; then take the allocation
            // from calls made AFTER the publication, so the object inspected
            // below is the compiled body's. What is checked is unchanged.
            const ATTEMPTS: usize = 400;
            const CALLS_PER_ATTEMPT: usize = 50;
            let compiled = |vm: &Vm| {
                vm.shared
                    .jit
                    .jit_cache
                    .read()
                    .get(class, "allocate", descriptor, class_id)
                    .is_some()
            };
            let mut attempts = 0;
            while !compiled(&vm) && attempts < ATTEMPTS {
                for _ in 0..CALLS_PER_ATTEMPT {
                    allocated = vm
                        .invoke(class, "allocate", descriptor, &[])
                        .expect("allocation method must return normally");
                }
                attempts += 1;
            }
            assert!(
                compiled(&vm),
                "allocate must be compiled before inspecting its TLAB allocation \
                 (not published after {} calls)",
                1200 + attempts * CALLS_PER_ATTEMPT
            );
            for _ in 0..4 {
                allocated = vm
                    .invoke(class, "allocate", descriptor, &[])
                    .expect("allocation method must return normally");
            }

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
