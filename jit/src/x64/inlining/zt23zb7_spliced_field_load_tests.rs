//! zt t23/zb7 (stage 2d-ii, spliced bodies): the spliced compact reference
//! `getfield` (`Compiler::emit_spliced_inline_getfield`) under a
//! colored-store ZGC load barrier. The colored state is opened on the
//! test's own thread (zb6's `ZgcColoredFieldLoadTestOverride`): nothing
//! process-wide is armed or published.

use super::*;
use crate::x64::licm::{zgc_read_gate_flip_test_lock, ZgcColoredFieldLoadTestOverride};

const BASE: u64 = 0x0000_7F00_0000_0000;
const CELL: u64 = 0x0000_7F11_2233_4440;
const HELPER: usize = 0x7FFF_0000_2000;
/// The compact displacement; not the legacy payload's, so the compact
/// load's bytes are unique in the emission.
const C_OFF: u32 = 40;

/// `MOV RAX, [RAX + C_OFF]`, the compact reference load.
fn compact_load() -> Vec<u8> {
    let mut v = vec![0x48, 0x8B, 0x80];
    v.extend_from_slice(&C_OFF.to_le_bytes());
    v
}

fn compiler() -> Compiler {
    let mut c = crate::x64::emit::tests::test_compiler();
    c.helpers.getfield = HELPER;
    c.helpers.read_bounds_addr = 0x1000;
    c.heap_local_offset = 8;
    c.method_key = "Zb7.caller:(LZb7;)LZb7;".to_string();
    c
}

/// Emit the spliced `getfield` of field 0 (receiver in a frame slot).
/// `(admitted, bytes)`, with every helper-call rel32 and every RIP-relative
/// displacement zeroed (both depend on the buffer's own address).
fn emit(
    c: &mut Compiler,
    type_tag: u8,
    compact: Option<(u32, bool)>,
    trusted: bool,
    volatile_or_referent: bool,
) -> (bool, Vec<u8>) {
    let start = c.buf.pos();
    let calls = c.helper_call_patches.len();
    let rips = c.rip_abs_disp32_patches.len();
    let admitted = c.emit_spliced_inline_getfield(
        StackSlot::Frame(16),
        0,
        type_tag,
        compact,
        0,
        trusted,
        volatile_or_referent,
        false,
    );
    let mut code = c.buf.as_slice()[start..].to_vec();
    for &p in &c.helper_call_patches[calls..] {
        code[p - start..p - start + 4].fill(0);
    }
    for &(p, _) in &c.rip_abs_disp32_patches[rips..] {
        code[p - start..p - start + 4].fill(0);
    }
    (admitted, code)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn le32(bytes: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn after_compact_load(code: &[u8]) -> usize {
    let load = compact_load();
    code.windows(load.len())
        .position(|w| w == &load[..])
        .expect("the splice loads the compact field inline")
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

/// The unarmed spliced arm and the arm under the colored state, retrying
/// while a concurrent test moves the layout-replacement epoch both bake.
fn plain_and_colored(cell: Option<u64>) -> Option<(Vec<u8>, Vec<u8>)> {
    for _ in 0..8 {
        let epoch = cratonvm_types::layout_replace_epoch();
        let (ok, plain) = emit(&mut compiler(), b'L', Some((C_OFF, true)), true, false);
        assert!(ok, "the unarmed spliced arm emits");
        let colored = {
            let _open = ZgcColoredFieldLoadTestOverride::open(BASE, cell);
            let (ok, code) = emit(&mut compiler(), b'L', Some((C_OFF, true)), true, false);
            assert!(ok, "the colored spliced arm emits");
            code
        };
        if cratonvm_types::layout_replace_epoch() == epoch {
            return Some((plain, colored));
        }
    }
    None
}

/// The default process: the guarded spliced arm open, the colored arm
/// closed.
fn unarmed_default() -> bool {
    !zgc_inline_field_loads_gated() && guarded_inline_getfield_enabled()
}

/// Pinned color: the colored spliced arm is the unarmed spliced arm byte
/// for byte with zb4's 31-byte decode right after the compact load
/// (forward jumps over it exactly 31 longer). This is also the byte pin of
/// the unarmed spliced arm: it has no decode and nothing else moved.
#[test]
fn zt23zb7_the_colored_spliced_getfield_is_the_unarmed_arm_plus_the_decode() {
    let _flip = zgc_read_gate_flip_test_lock();
    if !unarmed_default() {
        return;
    }
    let Some((plain, colored)) = plain_and_colored(None) else {
        return;
    };
    let decode = zgc_colored_slot_decode_bytes(BASE);
    assert!(
        !contains(&plain, &decode),
        "the unarmed splice emits no decode"
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
/// on the join; it replaces the unarmed arm's 5-byte `JMP done`, and
/// everything else is the unarmed spliced arm's.
#[test]
fn zt23zb7_the_flip_spliced_getfield_has_the_head_and_the_helper_slow_branch() {
    let _flip = zgc_read_gate_flip_test_lock();
    if !unarmed_default() {
        return;
    }
    let Some((plain, flip)) = plain_and_colored(Some(CELL)) else {
        return;
    };
    let head = zgc_colored_flip_head_bytes(CELL, BASE);
    let at = after_compact_load(&plain);
    assert_eq!(plain[at], 0xE9, "the unarmed compact arm ends in JMP done");
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

/// Refused shapes under the colored state: `false` and NOTHING emitted
/// (the caller keeps its helper-only sequence) for a volatile / referent
/// read, an untrusted receiver, a site with no compact row and a
/// non-reference descriptor. zt t32 (section 7): a primitive row of a
/// trusted receiver is the armed primitive arm instead (inline, no decode).
#[test]
fn zt23zb7_refused_spliced_shapes_emit_nothing_under_colored_stores() {
    let _open = ZgcColoredFieldLoadTestOverride::open(BASE, Some(CELL));
    let cases: [(u8, Option<(u32, bool)>, bool, bool); 4] = [
        (b'L', Some((C_OFF, true)), true, true),
        (b'L', Some((C_OFF, true)), false, false),
        (b'L', None, true, false),
        (b'I', Some((C_OFF, true)), true, false),
    ];
    for (tag, compact, trusted, vr) in cases {
        let mut c = compiler();
        let (ok, code) = emit(&mut c, tag, compact, trusted, vr);
        assert!(
            !ok,
            "tag={tag} compact={compact:?} trusted={trusted} vr={vr}: refused"
        );
        assert!(code.is_empty(), "a refused splice site emits nothing");
        assert_eq!(
            spliced_colored_ref_getfield_base(compact, tag, vr, trusted),
            None,
            "the predicate refuses it too"
        );
    }
    assert_eq!(
        spliced_colored_ref_getfield_base(Some((C_OFF, true)), b'[', false, true),
        guarded_inline_getfield_enabled().then_some(BASE),
        "an array-typed reference field is admitted"
    );
    if crate::x64::licm::zgc_armed_inline_primitive_fields_enabled()
        && guarded_inline_getfield_enabled()
    {
        let mut c = compiler();
        let (ok, code) = emit(&mut c, b'I', Some((C_OFF, false)), true, false);
        assert!(ok && !code.is_empty(), "armed primitive: inline");
        let decode = zgc_colored_slot_decode_bytes(BASE);
        assert!(!contains(&code, &decode), "no decode for a primitive");
    }
}

/// Unarmed and without the test seam (every default run): no spliced site
/// has a colored base, so the old guarded arm is all there is.
#[test]
fn zt23zb7_an_unarmed_process_offers_no_spliced_colored_base() {
    let _flip = zgc_read_gate_flip_test_lock();
    if zgc_inline_field_loads_gated() {
        return;
    }
    assert_eq!(
        spliced_colored_ref_getfield_base(Some((C_OFF, true)), b'L', false, true),
        None
    );
}
