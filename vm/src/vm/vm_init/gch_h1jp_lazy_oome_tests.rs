use super::Vm;
use crate::config::VmConfig;

/// gch h1/jp (`gcg-g1o-embedded-vms-never-preallocate-the-oome`): the
/// lazy one-shot of `Vm::invoke` is a real-JDK, booted VM's only; a
/// synthetic VM at level 4 neither tries nor spends its attempt, and a
/// caller that owns the preallocation turns the one-shot off.
#[test]
fn gch_h1jp_the_lazy_oome_preallocation_is_a_booted_real_jdk_vms_only() {
    let mut vm = Vm::new(VmConfig::default());
    assert!(
        vm.shared.config.java_home.is_none(),
        "fixture: a synthetic VM"
    );
    vm.shared.set_init_level(4);
    let _ = vm.invoke("gch/h1jp/Missing", "m", "()V", &[]);
    let attempted = |vm: &Vm| {
        vm.shared
            .mem
            .alloc_ladder
            .singleton_oom_attempted
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    assert!(!attempted(&vm), "a synthetic VM does not spend the attempt");
    assert!(vm.shared.mem.singleton_oom.read().is_none());
    vm.own_the_oome_preallocation();
    assert!(attempted(&vm), "the launcher's opt-out");
}
