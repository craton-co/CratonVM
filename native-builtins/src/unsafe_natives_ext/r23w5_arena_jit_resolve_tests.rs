use super::*;

/// The public resolve answers the block's own bytes for an access that
/// fits, and `None` past its end, for an untagged address, and after a
/// free -- the cases the FFM element door falls back on.
#[test]
fn resolve_serves_live_fitting_accesses_only() {
    if unsafe_arena_jit_table_addr() == 0 {
        return; // `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0` in this environment.
    }
    let h = unsafe_arena_allocate(64);
    assert_ne!(h, 0);
    let Some(real) = unsafe_arena_jit_resolve(h, 8) else {
        return; // a legacy handle (the table was full): nothing to resolve
    };
    assert!(unsafe_arena_copy_in(h + 8, &0x1122_3344_5566_7788u64.to_le_bytes()));
    // SAFETY: `real` is the live 64-byte block's first byte; bytes 8..16
    // are inside it.
    let got = unsafe { ((real + 8) as *const u64).read_unaligned() };
    assert_eq!(got, 0x1122_3344_5566_7788, "the resolved bytes are the block's");
    assert_eq!(unsafe_arena_jit_resolve(h + 8, 8), Some(real + 8));
    assert_eq!(unsafe_arena_jit_resolve(h + 60, 8), None, "past the block's end");
    assert_eq!(unsafe_arena_jit_resolve(h + 56, 8), Some(real + 56), "the last 8 bytes");
    assert_eq!(unsafe_arena_jit_resolve(0x1000, 1), None, "an untagged address");
    unsafe_arena_free(h);
    assert_eq!(unsafe_arena_jit_resolve(h, 1), None, "a freed block");
}
