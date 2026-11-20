use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_time_tables(vm);
        }
    }
}

fn mock_on(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn of_pattern(ctx: &mut MockNativeContext, text: &str) -> ObjectRef {
    let pat = ctx.create_string(text);
    match native_dtf_of_pattern(ctx, &[Value::Object(Some(pat))]).unwrap() {
        Some(Value::Object(Some(f))) => f,
        other => panic!("expected a formatter, got {other:?}"),
    }
}

/// A hit answers the cached (rooted) formatter; another VM misses and
/// builds its own; the FIFO bound releases the evicted formatter's root.
#[test]
fn w19b_the_pattern_cache_is_per_vm_and_roots_its_formatters() {
    const VM_A: usize = 0x19B0_0B01;
    const VM_B: usize = 0x19B0_0B02;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = mock_on(VM_A);
    let base_a = a.global_root_count();
    let first = of_pattern(&mut a, "HH:mm");
    assert_eq!(a.global_root_count(), base_a + 1, "the formatter is rooted");
    assert_eq!(of_pattern(&mut a, "HH:mm"), first, "a hit answers it");
    assert_eq!(a.global_root_count(), base_a + 1, "a hit roots nothing");

    let mut b = mock_on(VM_B);
    let base_b = b.global_root_count();
    let other = of_pattern(&mut b, "HH:mm");
    assert_eq!(
        b.global_root_count(),
        base_b + 1,
        "VM B misses VM A's entry and caches its own formatter"
    );
    let pattern = match b.get_field(other, DTF_FIELD_PATTERN) {
        Value::Object(Some(s)) => b.read_string(s),
        _ => None,
    };
    assert_eq!(pattern.as_deref(), Some("HH:mm"));

    for i in 0..DTF_PATTERN_CACHE_CAP {
        of_pattern(&mut a, &format!("'w19b-{i}' HH"));
    }
    assert_eq!(
        a.global_root_count(),
        base_a + DTF_PATTERN_CACHE_CAP,
        "the bound holds: each eviction released its root"
    );
    assert_eq!(
        b.global_root_count(),
        base_b + 1,
        "VM A's evictions release nothing of VM B's"
    );
}

/// The factories that store a fresh `String` return a complete object.
#[test]
fn w19b_fresh_string_factories_store_their_string() {
    let mut ctx = mock_on(0x19B0_0B03);
    let zone = alloc_zone_id(&mut ctx, "Europe/Paris");
    let id = match ctx.get_field(zone, ZID_FIELD_ID) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    assert_eq!(id.as_deref(), Some("Europe/Paris"));
    let rules = alloc_zone_rules(&mut ctx, "UTC");
    let rules_id = match ctx.get_field(rules, ZR_FIELD_ZONE_ID) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    assert_eq!(rules_id.as_deref(), Some("UTC"));
    let ldt = alloc_local_date_time(&mut ctx, 2026, 9, 25, 1, 2, 3, 0);
    let zdt = zdt_with_utc_zone(&mut ctx, ldt);
    assert!(matches!(
        ctx.get_field(zdt, ZDT_FIELD_LDT),
        Value::Object(Some(_))
    ));
    assert!(matches!(
        ctx.get_field(zdt, ZDT_FIELD_ZONE),
        Value::Object(Some(_))
    ));
    assert_eq!(ctx.get_field(zdt, ZDT_FIELD_OFFSET), Value::Int(0));
}
