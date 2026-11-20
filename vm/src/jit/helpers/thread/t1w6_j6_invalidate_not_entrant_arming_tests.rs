use super::*;

fn vm() -> std::sync::Arc<SharedVm> {
    let config = crate::config::VmConfig {
        use_synthetic_jdk: true,
        ..Default::default()
    };
    std::sync::Arc::new(crate::vm::vm_init::SharedVm::new(config))    }