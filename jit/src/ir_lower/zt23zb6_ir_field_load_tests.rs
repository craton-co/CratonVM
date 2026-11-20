    use super::*;
    use crate::x64::{zgc_read_gate_flip_test_lock, ZgcColoredFieldLoadTestOverride};

    const BASE: u64 = 0x0000_7F00_0000_0000;
    const CELL: u64 = 0x0000_7F11_2233_4440;
    const HELPER: usize = 0x7FFF_0000_2000;
    /// Distinctive, so the guarded containment's `MOV reg, imm64` of it is
    /// recognisable in the bytes.
    const READ_BOUNDS: usize = 0x0000_5A5A_1234_5000;
    const C_OFF: u32 = 40;
    const PC: usize = 4;

    /// `MOV RAX, [RAX + C_OFF]`, the compact reference load.
    fn compact_load() -> Vec<u8> {
        let mut v = vec![0x48, 0x8B, 0x80];
        v.extend_from_slice(&C_OFF.to_le_bytes());
        v
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    fn le32(bytes: &[u8], at: usize) -> i32 {
        i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }



    /// The default process, with the guarded inline arm on.
    fn unarmed_default() -> bool {
        !crate::x64::zgc_inline_field_loads_gated()
            && !crate::x64::inline_getfield_enabled()
            && crate::x64::guarded_inline_getfield_enabled()
            && crate::x64::trusted_oop_receiver_getfield_enabled()
    }






    // JIT r23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_GETFIELD_COLD_TAIL`): the
    // compact load falls straight through to the join's store (no `JMP done`
    // after it, no helper call inline), one tail is deferred, and the tail --
    // emitted past the body -- holds the legacy read and the checked helper,
    // each ending in a `JMP` back to the join. The kill restores the inline
    // shape (`JMP done` right after the load). The colored arm never defers.
// JIT r23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_GETFIELD_GUARD_NON_NULL`):
    // `Guard(p != null)` ahead of `p.f` in one block (the null-base
    // predicate's pre-header shape): the read emits no `TEST RAX,RAX; JZ`.
    // Without the guard, or with `=0`, the null test is there. A guard
    // AFTER the read proves nothing.
