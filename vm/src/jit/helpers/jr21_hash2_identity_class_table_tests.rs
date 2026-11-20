//! JIT round 21, lane hash2 (`CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE`): the
//! per-VM identity-hash class verdict table (`JitRealm::identity_hash_classes`)
//! and the well-known query that hands its address to the compiler.
use super::*;
use crate::config::VmConfig;
use cratonvm_jit::ir_lower as jl;

fn ask(shared: &SharedVm, tag: i64, cid: u32) -> Option<i64> {
    vh_const_well_known_answer(shared, jl::vh_const_query(tag, cid, 0))
}

/// The first table query allocates it; the address is stable, per VM,
/// and starts with the capacity word the compiled test compares against.
#[test]
fn the_table_query_allocates_one_stable_table_per_vm() {
    let shared = SharedVm::new(VmConfig::default());
    let table = &shared.jit.identity_hash_classes;
    assert!(!table.is_allocated());
    // Before the table exists, recording is a no-op.
    table.record(5, jl::IDENTITY_HASH_CLASS_PROVEN);
    assert_eq!(table.verdict(5), jl::IDENTITY_HASH_CLASS_UNKNOWN);
    let addr = ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0).unwrap();
    assert_ne!(addr, 0);
    assert!(table.is_allocated());
    assert_eq!(
        ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0),
        Some(addr)
    );
    // Cast: the address handed back as the compiler reads it.
    assert_eq!(usize::try_from(addr).unwrap(), table.base_addr());
    // SAFETY: `addr` is the live table's base; its first 8 bytes are the
    // capacity word, written before the address was handed out.
    let cap = unsafe { std::ptr::read_unaligned(table.base_addr() as *const u64) };
    assert_eq!(
        usize::try_from(cap).unwrap(),
        jl::IDENTITY_HASH_CLASS_TABLE_CAPACITY
    );
    let other = SharedVm::new(VmConfig::default());
    let other_addr = ask(&other, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0).unwrap();
    assert_ne!(other_addr, addr, "one table per VM");
}

/// A verdict is filed once and never rewritten; ids past the capacity
/// have no byte.
#[test]
fn a_verdict_is_written_once() {
    let shared = SharedVm::new(VmConfig::default());
    let table = &shared.jit.identity_hash_classes;
    let _ = table.base_addr();
    table.record(42, jl::IDENTITY_HASH_CLASS_PROVEN);
    table.record(42, jl::IDENTITY_HASH_CLASS_REFUSED);
    assert_eq!(table.verdict(42), jl::IDENTITY_HASH_CLASS_PROVEN);
    table.record(43, jl::IDENTITY_HASH_CLASS_REFUSED);
    table.record(43, jl::IDENTITY_HASH_CLASS_PROVEN);
    assert_eq!(table.verdict(43), jl::IDENTITY_HASH_CLASS_REFUSED);
    let past = u32::try_from(jl::IDENTITY_HASH_CLASS_TABLE_CAPACITY).unwrap();
    table.record(past, jl::IDENTITY_HASH_CLASS_PROVEN);
    assert_eq!(table.verdict(past), jl::IDENTITY_HASH_CLASS_UNKNOWN);
    // SAFETY: the byte of class 42 lies inside the live table.
    let byte = unsafe {
        *((table.base_addr() + jl::IDENTITY_HASH_CLASS_TABLE_HEADER + 42) as *const u8)
    };
    assert_eq!(
        byte,
        jl::IDENTITY_HASH_CLASS_PROVEN,
        "the byte compiled code reads"
    );
}

/// The identity-class query files nothing for a class the store does not
/// hold (it may be defined later) and still answers `0`.
#[test]
fn an_absent_class_is_not_filed_as_refused() {
    let shared = SharedVm::new(VmConfig::default());
    let _ = shared.jit.identity_hash_classes.base_addr();
    assert_eq!(
        ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG, 61_999),
        Some(0)
    );
    assert_eq!(
        shared.jit.identity_hash_classes.verdict(61_999),
        jl::IDENTITY_HASH_CLASS_UNKNOWN
    );
}
