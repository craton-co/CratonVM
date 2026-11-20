//! zt t23/zb6 (stage 2d-ii): the top-level compact reference `getfield`
//! under a colored-store ZGC load barrier. The colored state is opened on
//! the test's own thread (`ZgcColoredFieldLoadTestOverride`): nothing
//! process-wide is armed or published, so the other tests of this binary
//! keep their emission.

use super::*;
use crate::x64::licm::{zgc_read_gate_flip_test_lock, ZgcColoredFieldLoadTestOverride};

const BASE: u64 = 0x0000_7F00_0000_0000;
const CELL: u64 = 0x0000_7F11_2233_4440;
const HELPER: usize = 0x7FFF_0000_2000;
/// The compact displacement; not `HEADER_SIZE + PAYLOAD64` (24), so the
/// compact load's bytes differ from the legacy arm's.
const C_OFF: u32 = 40;
/// `getfield #1` at pc 0.
const CODE: [u8; 3] = [0xb4, 0x00, 0x01];

/// `MOV RAX, [RAX + C_OFF]`, the compact reference load.
fn compact_load() -> Vec<u8> {
    let mut v = vec![0x48, 0x8B, 0x80];
    v.extend_from_slice(&C_OFF.to_le_bytes());
    v
}

/// A fresh compiler at a `getfield` of a compact reference field (slot
/// 0, descriptor `L`) whose receiver sits in a frame slot, marked an oop
/// by the exact tracker (`trusted`) or not; the helper and the context
/// slot wired.
fn compiler_at_ref_getfield(volatile: bool, trusted: bool) -> Compiler {
    let mut c = crate::x64::emit::tests::test_compiler();
    c.helpers.getfield = HELPER;
    c.helpers.read_bounds_addr = 0x1000;
    c.heap_local_offset = 8;
    c.method_key = "Zb6.f:(LZb6;)LZb6;".to_string();
    c.compact_field_off.insert(0, (C_OFF, true));
    c.field_info.push((0, 0, b'L'));
    c.field_info_idx.insert(0, 0);
    if volatile {
        c.volatile_field_pcs.insert(0);
    }
    c.stack.push(StackSlot::Frame(16));
    c.stack_oop_marks.push(trusted);
    c
}

/// Walk the `getfield`; the emitted bytes with every helper-call rel32
/// zeroed (they depend on the buffer's own address).
fn walk(c: &mut Compiler) -> (WalkStep, Vec<u8>) {
    let start = c.buf.pos();
    let calls = c.helper_call_patches.len();
    let rips = c.rip_abs_disp32_patches.len();
    let step = c.walk_field_arm(
        &CODE,
        CODE.len(),
        0xb4,
        0,
        &mut false,
        &[false; 3],
        &[true, false, false],
    );
    let mut code = c.buf.as_slice()[start..].to_vec();
    for &p in &c.helper_call_patches[calls..] {
        code[p - start..p - start + 4].fill(0);
    }
    // zt t23/zb7: the layout-epoch guard's RIP-relative form (in reach on
    // a near-globals host) bakes a displacement from the buffer's address.
    for &(p, _) in &c.rip_abs_disp32_patches[rips..] {
        code[p - start..p - start + 4].fill(0);
    }
    (step, code)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn le32(bytes: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The end of the compact load in `code` (the unarmed arm's bytes).
fn after_compact_load(code: &[u8]) -> usize {
    let load = compact_load();
    code.windows(load.len())
        .position(|w| w == &load[..])
        .expect("the arm loads the compact field inline")
        + load.len()
}

/// `a` and `b` are equal except inside rel32 fields of `b` that are
/// exactly `shift` larger: forward jumps across `shift` inserted bytes.
fn assert_equal_but_shifted_rel32(a: &[u8], b: &[u8], shift: i32) {
    assert_eq!(a.len(), b.len());
    let mut i = 0;
    while i < a.len() {
        if a[i] == b[i] {
            i += 1;
            continue;
        }
        let k = (i.saturating_sub(3)..=i)
            .find(|&k| k + 4 <= a.len() && le32(b, k).wrapping_sub(le32(a, k)) == shift)
            .unwrap_or_else(|| panic!("byte {i} differs outside a shifted rel32"));
        i = k + 4;
    }
}

/// Emit the unarmed arm and the arm under `colored`, retrying while a
/// concurrent test moves the layout-replacement epoch both arms bake.
/// `None` when the epoch never held still (not this test's question).
fn plain_and_colored(base: u64, cell: Option<u64>) -> Option<(Vec<u8>, Vec<u8>)> {
    for _ in 0..8 {
        let epoch = cratonvm_types::layout_replace_epoch();
        let mut plain = compiler_at_ref_getfield(false, true);
        let (step, plain_code) = walk(&mut plain);
        assert!(matches!(step, WalkStep::Next(3)), "the unarmed arm emits");
        let colored_code = {
            let _open = ZgcColoredFieldLoadTestOverride::open(base, cell);
            let mut c = compiler_at_ref_getfield(false, true);
            let (step, code) = walk(&mut c);
            assert!(matches!(step, WalkStep::Next(3)), "the colored arm emits");
            assert_eq!(c.stack.len(), 1, "the value replaces the receiver");
            assert_eq!(c.stack_oop_marks.last(), Some(&true), "and is an oop");
            code
        };
        if cratonvm_types::layout_replace_epoch() == epoch {
            return Some((plain_code, colored_code));
        }
    }
    None
}

/// The default process: the plain arm is open (its guarded form), the
/// colored arm closed.
fn unarmed_default() -> bool {
    !zgc_inline_field_loads_gated()
        && !inline_getfield_enabled()
        && guarded_inline_getfield_enabled()
}

/// Pinned color: the colored arm is the unarmed arm byte for byte with
/// zb4's 31-byte decode right after the compact load (forward jumps over
/// it lengthened by 31, nothing else); the unarmed arm has no decode.
#[test]
fn zt23zb6_the_colored_getfield_is_the_unarmed_arm_plus_the_decode() {
    let _flip = zgc_read_gate_flip_test_lock();
    if !unarmed_default() {
        return;
    }
    let Some((plain, colored)) = plain_and_colored(BASE, None) else {
        return;
    };
    let decode = zgc_colored_slot_decode_bytes(BASE);
    assert!(
        !contains(&plain, &decode),
        "the unarmed arm emits no decode"
    );
    let at = after_compact_load(&plain);
    let n = decode.len();
    assert_eq!(colored.len(), plain.len() + n);
    assert_eq!(
        &colored[at..at + n],
        &decode[..],
        "the decode after the load"
    );
    assert_eq!(&colored[at + n..], &plain[at..], "the rest unchanged");
    // Cast: the 31-byte decode length
    assert_equal_but_shifted_rel32(&plain[..at], &colored[..at], n as i32);
}

/// Color flip: zb5's 55-byte head right after the compact load, its
/// `.slow` a `JMP` into the checked `jit_getfield` tail, both of its exits
/// on one join; it replaces the unarmed arm's 5-byte `JMP done`, and
/// everything else is the unarmed arm's.
#[test]
fn zt23zb6_the_flip_getfield_is_the_head_with_the_helper_as_its_slow_branch() {
    let _flip = zgc_read_gate_flip_test_lock();
    if !unarmed_default() {
        return;
    }
    let Some((plain, flip)) = plain_and_colored(BASE, Some(CELL)) else {
        return;
    };
    let head = zgc_colored_flip_head_bytes(CELL, BASE);
    let at = after_compact_load(&plain);
    assert_eq!(plain[at], 0xE9, "the unarmed compact arm ends in JMP done");
    // head + `JMP .slow` - `JMP done`
    let added = ZGC_COLORED_FLIP_HEAD_LEN;
    assert_eq!(flip.len(), plain.len() + added);
    assert_eq!(&flip[at..at + 5], &head[..5], "TEST + JNS opcode");
    assert_eq!(
        &flip[at + 9..at + 51],
        &head[9..51],
        "cell, bad test, decode"
    );
    let slow_jmp = at + ZGC_COLORED_FLIP_HEAD_LEN;
    assert_eq!(flip[slow_jmp], 0xE9, ".slow: JMP to the helper tail");
    assert_eq!(
        &flip[slow_jmp + 5..],
        &plain[at + 5..],
        "the rest unchanged"
    );
    // Cast: the 55-byte growth
    assert_equal_but_shifted_rel32(&plain[..at], &flip[..at], added as i32);
    // Cast: offsets inside one small buffer
    let target = |rel_at: usize| (rel_at as i64 + 4 + i64::from(le32(&flip, rel_at))) as usize;
    let jns = target(at + ZGC_COLORED_FLIP_JNS_PATCH);
    let good = target(at + ZGC_COLORED_FLIP_JMP_PATCH);
    let slow = target(slow_jmp + 1);
    assert_eq!(jns, good, "plain / null and good words share the join");
    assert!(slow > slow_jmp + 5 && slow < jns, ".slow lands in the tail");
    // The checked helper is called between `.slow`'s target and the join
    // (a masked `CALL rel32`, or the far form's imm64).
    let far = (HELPER as u64).to_le_bytes();
    let call_at = (slow..jns)
        .find(|&p| flip[p..].starts_with(&[0xE8, 0, 0, 0, 0]) || flip[p..].starts_with(&far));
    assert!(
        call_at.is_some(),
        "a helper call between .slow and the join"
    );
    assert!(
        !contains(&flip, &zgc_colored_slot_decode_bytes(BASE)),
        "no pinned decode"
    );
}

/// Refused shapes keep the helper under the colored state: no compact
/// inline load, no decode, for a volatile field or an untracked receiver.
#[test]
fn zt23zb6_volatile_and_untrusted_sites_keep_the_helper() {
    let _open = ZgcColoredFieldLoadTestOverride::open(BASE, None);
    let decode = zgc_colored_slot_decode_bytes(BASE);
    for (volatile, trusted) in [(true, true), (false, false)] {
        let mut c = compiler_at_ref_getfield(volatile, trusted);
        let (_, code) = walk(&mut c);
        assert!(
            !contains(&code, &decode),
            "volatile={volatile} trusted={trusted}: no inline decode"
        );
        assert!(
            !contains(&code, &compact_load()),
            "volatile={volatile} trusted={trusted}: no compact inline load"
        );
    }
}

/// Every refusal of the shape predicate, and its admissions.
#[test]
fn zt32_the_armed_primitive_predicate_admits_only_a_trusted_plain_primitive() {
    let p = armed_primitive_field_shape_admitted;
    if !crate::x64::licm::zgc_armed_inline_primitive_fields_enabled() {
        return;
    }
    for tag in *b"IJFDZBCS" {
        assert!(p(false, tag, false, false, true, true), "{}", tag as char);
    }
    assert!(!p(true, b'I', false, false, true, true), "a reference row");
    assert!(!p(false, b'L', false, false, true, true), "a ref type");
    assert!(!p(false, b'[', false, false, true, true), "an array type");
    assert!(!p(false, b'I', true, false, true, true), "volatile");
    assert!(!p(false, b'I', false, true, true, true), "the raw mode");
    assert!(!p(false, b'I', false, false, false, true), "guarded off");
    assert!(!p(false, b'I', false, false, true, false), "untrusted");
}

#[test]
fn zt23zb6_the_shape_predicate_admits_only_a_trusted_plain_reference() {
    let admitted = colored_ref_getfield_shape_admitted;
    assert!(admitted(true, b'L', false, false, true, true));
    assert!(admitted(true, b'[', false, false, true, true));
    // a primitive row, a non-reference descriptor
    assert!(!admitted(false, b'L', false, false, true, true));
    assert!(!admitted(true, b'I', false, false, true, true));
    // volatile, the raw opt-in, the guarded arm off, an untrusted receiver
    assert!(!admitted(true, b'L', true, false, true, true));
    assert!(!admitted(true, b'L', false, true, true, true));
    assert!(!admitted(true, b'L', false, false, false, true));
    assert!(!admitted(true, b'L', false, false, true, false));
}

/// Unarmed and without the test seam (every default run): the plain arm
/// is open and no field site has a colored base.
#[test]
fn zt23zb6_an_unarmed_process_opens_no_colored_field_arm() {
    let _flip = zgc_read_gate_flip_test_lock();
    if zgc_inline_field_loads_gated() {
        return;
    }
    assert_eq!(zgc_colored_inline_field_load_base(), None);
    assert_eq!(zgc_colored_field_flip_bad_mask_cell(), None);
    let c = compiler_at_ref_getfield(false, true);
    assert_eq!(c.sp_colored_ref_getfield_base(0, true), None);
}
