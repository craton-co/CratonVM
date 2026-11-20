use super::{
    native_unsafe_allocate_memory, native_unsafe_allocate_memory_realloc,
    unsafe_arena_contains, unsafe_arena_free,
};
use cratonvm_native_api::test_mock::MockNativeContext;
use cratonvm_types::Value;

/// Both `allocateMemory` bodies hand out ARENA addresses, never the heap
/// address of a managed array: a heap address in a Java `long` is an
/// unminted smuggle the GC roots but never remaps
/// (`i1-L4-operand-stack-roots-unminted-primitive-longs`). And a block
/// is readable by the raw accessors only if the arena knows it.
#[test]
fn allocate_and_reallocate_return_arena_addresses() {
    let _arena = super::arena_layout_test_lock();
    let mut ctx = MockNativeContext::new();
    let this = Value::Object(None);
    let Ok(Some(Value::Long(addr))) =
        native_unsafe_allocate_memory(&mut ctx, &[this, Value::Long(64)])
    else {
        panic!("allocateMemory(64) must return a long");
    };
    assert!(
        unsafe_arena_contains(addr),
        "0x{addr:x} is not an arena block"
    );
    let Ok(Some(Value::Long(addr2))) = native_unsafe_allocate_memory_realloc(
        &mut ctx,
        &[this, Value::Long(addr), Value::Long(128)],
    ) else {
        panic!("reallocateMemory(addr, 128) must return a long");
    };
    assert!(
        unsafe_arena_contains(addr2),
        "0x{addr2:x} is not an arena block"
    );
    unsafe_arena_free(addr2);
    // HotSpot: allocateMemory(0) returns 0 (the old body allocated a
    // one-element array and returned its address).
    assert_eq!(
        native_unsafe_allocate_memory(&mut ctx, &[this, Value::Long(0)]).ok(),
        Some(Some(Value::Long(0)))
    );
}
