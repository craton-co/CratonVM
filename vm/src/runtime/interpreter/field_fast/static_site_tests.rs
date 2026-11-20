use super::*;
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

fn vm() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(crate::config::VmConfig::default()))
}

fn thread_with_frame() -> JvmThread {
    let mut thread = JvmThread::new(ThreadId(4343), "static-site");
    thread.frames.push(Frame::new(
        ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    thread
}

fn static_int(declaring: ClassId, is_volatile: bool) -> ResolvedField {
    ResolvedField {
        declaring_class_id: declaring,
        field_index: 0,
        is_static: true,
        is_volatile,
        is_final: false,
        is_reference: false,
        desc_byte: b'I',
    }
}

/// A filled site answers `getstatic` with the current value of the static,
/// read through `get_static_shared`, and a later store is seen.
#[test]
fn a_filled_site_serves_the_current_static_value() {
    if cratonvm_classloading::any_class_redefined() {
        return; // the site tables are latched off for this process
    }
    let shared = vm();
    let decl = ClassId::new(90_001);
    crate::vm::set_static_shared(&shared, decl, 0, Value::Int(42));
    let mut thread = thread_with_frame();
    // Other tests load classes concurrently and move the epochs, which
    // drops a fill; retry until one sticks.
    let mut served = false;
    for _ in 0..1000 {
        let epochs = FieldSiteCache::epochs_now();
        thread.site_tables.static_field_sites.put(
            ClassId::new(0),
            7,
            epochs,
            static_int(decl, false),
        );
        if matches!(getstatic_site_hit(&shared, &mut thread, 0, 7), Ok(true)) {
            served = true;
            break;
        }
    }
    assert!(served, "a filled static site must be served");
    assert_eq!(thread.frames[0].stack.pop().ok(), Some(Value::Int(42)));

    crate::vm::set_static_shared(&shared, decl, 0, Value::Int(43));
    if matches!(getstatic_site_hit(&shared, &mut thread, 0, 7), Ok(true)) {
        assert_eq!(thread.frames[0].stack.pop().ok(), Some(Value::Int(43)));
    }
}

/// An empty site declines without touching the stack.
#[test]
fn an_unfilled_site_declines() {
    let shared = vm();
    let mut thread = thread_with_frame();
    assert!(matches!(
        getstatic_site_hit(&shared, &mut thread, 0, 9),
        Ok(false)
    ));
    assert_eq!(thread.frames[0].stack.len(), 0);
}

/// Wave 5: the `System` screen is per VM. Two VMs that recorded different
/// ids for `java/lang/System` each answer for their own id only (the
/// deleted process-global latch answered every VM with the last one).
#[test]
fn the_system_screen_answers_per_vm() {
    use crate::runtime::interpreter::opcodes::static_may_be_system;
    use std::sync::atomic::Ordering as O;
    let a = vm();
    let b = vm();
    a.classes.system_class_id.store(70_001, O::Relaxed);
    b.classes.system_class_id.store(70_002, O::Relaxed);
    assert!(static_may_be_system(&a, ClassId::new(70_001)));
    assert!(!static_may_be_system(&a, ClassId::new(70_002)));
    assert!(static_may_be_system(&b, ClassId::new(70_002)));
    assert!(!static_may_be_system(&b, ClassId::new(70_001)));
}

/// Wave 5: one entry serves `putstatic` (the store lands and pops its
/// operand) and a volatile `getstatic`; a final field declines the
/// `putstatic` hit and leaves the operand for the full handler.
#[test]
fn a_filled_site_serves_putstatic_and_volatile_reads_but_no_final_store() {
    if cratonvm_classloading::any_class_redefined() {
        return; // the site tables are latched off for this process
    }
    let shared = vm();
    let decl = ClassId::new(90_003);
    crate::vm::set_static_shared(&shared, decl, 0, Value::Int(1));
    let mut thread = thread_with_frame();
    let mut served = false;
    for _ in 0..1000 {
        let epochs = FieldSiteCache::epochs_now();
        thread.site_tables.static_field_sites.put(
            ClassId::new(0),
            13,
            epochs,
            static_int(decl, true),
        );
        let _ = thread.frames[0].stack.push(Value::Int(77));
        if matches!(putstatic_site_hit(&shared, &mut thread, 0, 13), Ok(true)) {
            served = true;
            break;
        }
        let _ = thread.frames[0].stack.pop();
    }
    assert!(served, "a filled static site must serve putstatic");
    assert_eq!(
        thread.frames[0].stack.len(),
        0,
        "the store pops its operand"
    );
    assert_eq!(
        crate::vm::get_static_shared(&shared, decl, 0),
        Value::Int(77)
    );
    if matches!(getstatic_site_hit(&shared, &mut thread, 0, 13), Ok(true)) {
        assert_eq!(thread.frames[0].stack.pop().ok(), Some(Value::Int(77)));
    }

    let mut final_field = static_int(decl, false);
    final_field.is_final = true;
    let epochs = FieldSiteCache::epochs_now();
    thread
        .site_tables
        .static_field_sites
        .put(ClassId::new(0), 14, epochs, final_field);
    let _ = thread.frames[0].stack.push(Value::Int(5));
    assert!(matches!(
        putstatic_site_hit(&shared, &mut thread, 0, 14),
        Ok(false)
    ));
    assert_eq!(
        thread.frames[0].stack.len(),
        1,
        "a declined store pops nothing"
    );
    assert_eq!(
        crate::vm::get_static_shared(&shared, decl, 0),
        Value::Int(77)
    );
}

/// The fill refuses a class that is not `Initialized` (here: unknown to the
/// class manager) — for a volatile field too, which wave 5 otherwise
/// admits — even when `field_sites` admits the site.
#[test]
fn fill_requires_an_initialized_class_and_a_plain_field() {
    let shared = vm();
    let mut thread = thread_with_frame();
    let decl = ClassId::new(90_002);
    for field in [static_int(decl, false), static_int(decl, true)] {
        let epochs = FieldSiteCache::epochs_now();
        thread
            .field_sites
            .put(ClassId::new(0), 11, epochs, field.clone());
        fill_static_site(&shared, &mut thread, 0, 11, &field);
        assert!(thread
            .site_tables
            .static_field_sites
            .get(ClassId::new(0), 11)
            .is_none());
    }
    // Not admitted by `field_sites` at all: never filled.
    let other = static_int(decl, false);
    fill_static_site(&shared, &mut thread, 0, 12, &other);
    assert!(thread
        .site_tables
        .static_field_sites
        .get(ClassId::new(0), 12)
        .is_none());
}
