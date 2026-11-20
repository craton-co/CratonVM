//! Round 16, lane mega, MG2-2 step 1: a class cell refused for the
//! table's column budget, or for a column past the receiver's array, is
//! reported (the VM's census counts both) instead of vanishing.
use super::*;
use crate::{ExecutableBuffer, JitCache};

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
    let cid = cratonvm_types::ClassId::new(0x1601);
    let class: Arc<str> = Arc::from("R16Mega");
    let method: Arc<str> = Arc::from(name);
    let desc: Arc<str> = Arc::from("()V");
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    cache.get(&class, &method, &desc, cid).expect("published")
}

#[test]
fn a_budget_refusal_is_reported_and_an_array_that_fits_is_not() {
    let cache = JitCache::new();
    let body = published(&cache, "budget");
    let entry = body.entry_ptr() as u64;
    let table = MegaDispatchTable::new();
    table.writer.lock().columns = MEGA_CLASS_COLUMNS_TOTAL_MAX - 6;
    // An eight-column vtable does not fit in the six columns left.
    let over = table.install_with_class_slot(40, 1, Some((2, 8)), entry, false, false);
    assert!(over.hashed, "the hashed ways still take it");
    assert!(!over.cell && over.cell_budget_refused && !over.cell_past_array);
    assert_eq!(table.lookup_class_cell(40, 1, 2), None);
    // A four-column one does, and is not reported.
    let fits = table.install_with_class_slot(41, 1, Some((2, 4)), entry, false, false);
    assert!(fits.cell && !fits.cell_budget_refused && !fits.cell_past_array);
    assert_eq!(
        table.writer.lock().columns,
        MEGA_CLASS_COLUMNS_TOTAL_MAX - 2
    );
    // A deeper column on the existing four-column array: past it.
    let past = table.install_with_class_slot(41, 2, Some((5, 8)), entry, false, false);
    assert!(!past.cell && past.cell_past_array && !past.cell_budget_refused);
    // A refusal for another reason (column 0, no column) reports nothing.
    let none = table.install_with_class_slot(42, 1, Some((0, 8)), entry, false, false);
    assert!(!none.cell && !none.cell_past_array && !none.cell_budget_refused);
    table.clear_entries();
    crate::defer_jit_owner(Some(body));
}
