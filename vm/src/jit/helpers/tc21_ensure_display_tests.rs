use super::*;
use crate::config::VmConfig;

/// tc21: `vh_const_ensure_display` publishes an ordinary class's display
/// and nothing for a lambda-proxy id (`>= 0x8000_0000`, absent from the
/// class store); the proxy band is screened before the class manager's
/// lock is taken.
#[test]
fn tc21_ensure_display_publishes_an_ordinary_class_and_skips_proxy_ids() {
    let shared = SharedVm::new(VmConfig::default());
    if !shared.jit.supers_displays.enabled() {
        return;
    }
    let real = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(real) = cm.try_ensure_synthetic_class("cratonvm/test/tc21/EnsureReal", 0) else {
            return; // --real-jdk policy in the environment
        };
        if cm.class_store.get(real).is_none() {
            return;
        }
        cm.set_superclass(real, None);
        cm.set_interfaces(real, Vec::new());
        real
    };
    let d = &shared.jit.supers_displays;
    assert!(!d.is_published(real.as_u32()));
    vh_const_ensure_display(&shared, real.as_u32());
    assert!(d.is_published(real.as_u32()), "an ordinary class is published");
    for proxy in [0x8000_0000_u32, 0x8000_0001, u32::MAX] {
        vh_const_ensure_display(&shared, proxy);
        assert!(!d.is_published(proxy), "proxy id {proxy:#x}: nothing to publish");
    }
}
