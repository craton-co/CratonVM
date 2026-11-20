use super::*;
use crate::registry::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments, Value};

const VM: &str = "jdk/internal/misc/VM";

fn cb(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}



fn registered(mode: CompatibilityMode, real_jdk: bool, rows: HotSpotAlignments) -> Vec<bool> {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_real_jdk(real_jdk);
    r.set_hotspot_alignments(rows);
    {
        r.register(VM, "initLevel", "()I", cb, NativeKind::SyntheticStub);
        r.register(VM, "isBooted", "()Z", cb, NativeKind::SyntheticStub);
        r.register(VM, "isJavaLangInvokeInited", "()Z", cb, NativeKind::SyntheticStub);
    };
    [("initLevel", "()I"), ("isBooted", "()Z"), ("isJavaLangInvokeInited", "()Z")]
        .iter()
        .map(|&(m, d)| r.kind_of(VM, m, d).is_some())
        .collect()
}


fn the_two_registrations_body() {
    let on = HotSpotAlignments::defaults();
    assert!(on.contains(Row::JdkVmInitLevel));
    let off = on.with(Row::JdkVmInitLevel, false);
    let compat = CompatibilityMode::Compatible;
    assert_eq!(registered(compat, true, on), vec![false, false, true]);
    // `-jdk-vm-init-level`: the natives come back (FBTAIL's static-reading
    // `initLevel` callback, the `isBooted` stand-in).
    assert_eq!(registered(compat, true, off), vec![true, true, true]);
    // The synthetic class library keeps them whatever the set says.
    assert_eq!(registered(compat, false, on), vec![true, true, true]);
}
