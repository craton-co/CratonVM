//! JIT round 21, lane hashlock (HASH2-21-1,
//! `CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM`): the per-VM equals verdict
//! table (`JitRealm::identity_equals_classes`), its queries, and its
//! pairing with the identity-hash class table.
use super::*;
use crate::config::VmConfig;
use cratonvm_jit::ir_lower as jl;

fn ask(shared: &SharedVm, tag: i64, cid: u32) -> Option<i64> {
    vh_const_well_known_answer(shared, jl::vh_const_query(tag, cid, 0))
}

/// The equals table query allocates one stable table per VM, distinct
/// from the identity-hash one, with the capacity word the shared compiled
/// test reads.
#[test]
fn the_equals_table_query_allocates_one_stable_table_per_vm() {
    let shared = SharedVm::new(VmConfig::default());
    let table = &shared.jit.identity_equals_classes;
    assert!(!table.is_allocated());
    let addr = ask(
        &shared,
        jl::WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG,
        0,
    )
    .unwrap();
    assert_ne!(addr, 0);
    assert_eq!(
        ask(
            &shared,
            jl::WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG,
            0
        ),
        Some(addr)
    );
    // Cast: the address handed back as the compiler reads it.
    assert_eq!(usize::try_from(addr).unwrap(), table.base_addr());
    // SAFETY: the live table's first 8 bytes are its capacity word.
    let cap = unsafe { std::ptr::read_unaligned(table.base_addr() as *const u64) };
    assert_eq!(
        usize::try_from(cap).unwrap(),
        jl::IDENTITY_HASH_CLASS_TABLE_CAPACITY
    );
    let hash_addr = ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0).unwrap();
    assert_ne!(hash_addr, addr, "two tables");
}


/// `java/lang/Object` itself (id 0, the arm's own exact compare) and a
/// lambda / sentinel id are refused outright; a class the store does not
/// hold answers `0` and stays unknown, so a later ask can still prove it.
#[test]
fn unprovable_classes_answer_zero_and_absent_ones_stay_unknown() {
    let shared = SharedVm::new(VmConfig::default());
    let table = &shared.jit.identity_equals_classes;
    let _ = table.base_addr();
    assert_eq!(
        class_equals_identity_verdict(&shared, ClassId::new(0)),
        Some(false)
    );
    assert_eq!(
        class_equals_identity_verdict(&shared, ClassId::new(0x8000_0001)),
        Some(false)
    );
    assert_eq!(
        ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TAG, 61_999),
        Some(0)
    );
    assert_eq!(table.verdict(61_999), jl::IDENTITY_HASH_CLASS_UNKNOWN);
}

/// JIT round 21 (lane tlsub, wave 21; correctness): a bridge `get()Object`
/// / `set(Object)V` is an override (javac's covariant-override bridge), and
/// the `set` helper screens `set` overrides before its store and its
/// private miss arm.
#[test]
fn jr21_tlsub_bridges_are_overrides_and_set_screens_its_overrides() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let slice = |from: &str, to: &str| -> String {
        let a = src.find(from).expect(from);
        let b = a + src[a..].find(to).expect(to);
        src[a..b].to_string()
    };
    let kind = slice(
        "\npub fn tl_get_compute_kind(",
        "\npub fn tl_class_declares_set(",
    );
    let set = slice(
        "\npub fn tl_class_declares_set(",
        "\npub fn tl_get_receiver_kind(",
    );
    for body in [&kind, &set] {
        assert!(
            !body.contains("is_bridge"),
            "a bridge must count as an override"
        );
    }
    let helper = slice(
        "\npub unsafe fn jit_thread_local_set_direct_body(",
        "\npub(in crate::jit) unsafe extern \"C\" fn jit_thread_local_get_direct(",
    );
    let screen = helper.find("tl_set_receiver_ok(").expect("the set screen");
    let find = helper.find("tl_find_entry(").expect("the probe");
    let private = helper.find("tl_miss_private_ok(").expect("the miss arm");
    assert!(screen < find && find < private);
    assert_eq!(TL_SET_MEMO_TAG, 1 << 31);
}
