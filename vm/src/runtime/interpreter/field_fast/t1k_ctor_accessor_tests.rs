use super::*;

/// What `op_putfield` stores, per descriptor; every shape it decodes by a
/// heuristic is refused.
#[test]
fn the_accessor_decodes_exactly_the_shapes_the_quickened_store_takes() {
    assert_eq!(
        accessor_store_value(b'I', CompactValue::int(7), 0),
        Some(Value::Int(7))
    );
    // Sub-int fields narrow, as `op_putfield` does.
    assert_eq!(
        accessor_store_value(b'B', CompactValue::int(0x1ff), 0),
        Some(super::super::narrow_int_to_field_type(Value::Int(0x1ff), b'B'))
    );
    assert_eq!(
        accessor_store_value(b'F', CompactValue::float(1.5), 0),
        Some(Value::Float(1.5))
    );
    assert_eq!(accessor_store_value(b'F', CompactValue::int(1), 0), None);
    assert_eq!(
        accessor_store_value(b'J', CompactValue::long(1 << 40), ValueStack::KIND_MARK_LONG),
        Some(Value::Long(1 << 40))
    );
    // An unmarked category-2 slot is the slow handler's.
    assert_eq!(accessor_store_value(b'J', CompactValue::long(5), 0), None);
    assert_eq!(accessor_store_value(b'D', CompactValue::long(5), 0), None);
    assert_eq!(
        accessor_store_value(b'L', CompactValue::null(), 0),
        Some(Value::Object(None))
    );
    // `Int(0)` in a reference field is a coercion the slow path owns.
    assert_eq!(accessor_store_value(b'L', CompactValue::int(0), 0), None);
    assert_eq!(accessor_store_value(b'V', CompactValue::int(0), 0), None);
}

fn gen_vm(on: bool) -> SharedVm {
    SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        interp_field_ctor_gen_g1: on,
        ..crate::config::VmConfig::default()
    })
}

fn legacy_site(header: &ObjectHeader, field_index: u32, desc_byte: u8) -> FastFieldSite {
    FastFieldSite {
        receiver_class_id: header.class_id,
        num_slots: header.num_slots(),
        offset: (HEADER_SIZE + field_index as usize * SLOT_SIZE) as u32,
        storage: None,
        field_index,
        desc_byte,
        is_reference: matches!(desc_byte, b'L' | b'['),
        final_put: FastFieldSite::PUT_ANY,
    }
}

/// On a Generational heap the stores land through `set_field`, all of
/// them or none: a store whose site is missing writes nothing.
#[test]
fn the_accessor_arm_makes_every_store_or_none_on_a_generational_heap() {
    if cratonvm_classloading::any_class_redefined() {
        return; // the site tables are latched off for this process
    }
    let shared = gen_vm(true);
    if !ctor_accessor_heap_admitted(&shared) {
        return; // a diagnostic or kill switch in this environment
    }
    let heap = &shared.mem.heap;
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let target = heap.alloc_object(ClassId::new(0), 0);
    // SAFETY: `obj` was just allocated on this heap.
    let header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
    if cratonvm_types::is_compact_object(header) {
        return; // this build lays class 0 out compact; the legacy shape is the one pinned here
    }
    let key = ClassId::new(987_201);
    let recv = obj.as_ptr() as u64;
    let stores = [
        (7u16, CompactValue::int(42), 0u8),
        (8u16, CompactValue::object(target.as_ptr() as u64), 0u8),
    ];
    // Only the first site: nothing is written.
    let mut sites = FastFieldSiteCache::new();
    let mut filled = false;
    for _ in 0..1000 {
        sites.put(key, 7, FieldSiteCache::epochs_now(), legacy_site(header, 0, b'I'));
        if sites.get(key, 7).is_some() {
            filled = true;
            break;
        }
    }
    assert!(filled, "a site must stick");
    assert!(!ctor_field_stores_accessor(&shared, &mut sites, key, recv, &stores, false));
    assert_ne!(heap.get_field(obj, 0), Value::Int(42), "all or nothing");
    // Both sites: both stores.
    let mut both = false;
    for _ in 0..1000 {
        let epochs = FieldSiteCache::epochs_now();
        sites.put(key, 7, epochs, legacy_site(header, 0, b'I'));
        sites.put(key, 8, epochs, legacy_site(header, 1, b'L'));
        if sites.get(key, 7).is_some() && sites.get(key, 8).is_some() {
            both = true;
            break;
        }
    }
    assert!(both, "both sites must stick");
    assert!(ctor_field_stores_accessor(&shared, &mut sites, key, recv, &stores, false));
    assert_eq!(heap.get_field(obj, 0), Value::Int(42));
    assert_eq!(heap.get_field(obj, 1), Value::Object(Some(target)));
    // A site filled for another receiver class refuses this receiver.
    let mut other = legacy_site(header, 0, b'I');
    other.receiver_class_id = ClassId::new(987_202);
    sites.put(key, 7, FieldSiteCache::epochs_now(), other);
    if sites.get(key, 7).is_some() {
        assert!(!ctor_field_stores_accessor(&shared, &mut sites, key, recv, &stores, false));
    }
}

/// The accessor arm is for the Generational and G1 heaps only.
#[test]
fn the_accessor_arm_is_not_admitted_on_zgc() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if shared.mem.heap.is_zgc() {
        assert!(!ctor_accessor_heap_admitted(&shared));
    }
}
