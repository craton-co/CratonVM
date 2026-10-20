// SPDX-License-Identifier: Apache-2.0
//! A premature VM readiness flag poisons the first real MethodHandleNatives
//! initializer. Exercise the shared launcher/embedding boot in both policies.
use cratonvm_types::Value;
use cratonvm_vm::config::{CompatibilityMode, VmConfig};
use cratonvm_vm::vm::Vm;

#[test]
fn real_jdk_boot_initializes_invoke_before_publishing_readiness() {
    let Ok(home) = std::env::var("JAVA_HOME") else {
        eprintln!("[jdk_boot_invoke_ready] JAVA_HOME is not set; skipping real-JDK test");
        return;
    };
    // VM construction and Java bootstrap use deep Rust frames. Keep this
    // integration check off the small stack reserved by the test harness.
    std::thread::Builder::new()
        .name("jdk-invoke-readiness-test".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
                let config = VmConfig::with_host_jdk_default()
                    .with_java_home(home.clone())
                    .with_compatibility_mode(mode);
                let mut vm = Vm::new(config);
                let phase1 = vm.boot_init_phase1();
                vm.boot_module_system(phase1);
                vm.shared.set_init_level(4);
                vm.boot_publish_jdk_booted()
                    .unwrap_or_else(|error| panic!("{mode:?} boot: {error}"));
                // Publishing twice must reuse the initialized class, rather than
                // calling VM's one-shot setter twice.
                vm.boot_publish_jdk_booted()
                    .unwrap_or_else(|error| panic!("{mode:?} repeated publication: {error}"));
                assert_eq!(
                    vm.invoke("jdk/internal/misc/VM", "isJavaLangInvokeInited", "()Z", &[])
                        .expect("readiness"),
                    Some(Value::Int(1)),
                    "{mode:?} readiness"
                );
                assert!(
                    matches!(
                        vm.invoke(
                            "java/lang/foreign/Linker",
                            "nativeLinker",
                            "()Ljava/lang/foreign/Linker;",
                            &[]
                        ),
                        Ok(Some(Value::Object(Some(_))))
                    ),
                    "{mode:?} first FFM linker use after boot"
                );
            }
        })
        .expect("spawn larger-stack real-JDK test")
        .join()
        .expect("real-JDK boot test thread");
}
