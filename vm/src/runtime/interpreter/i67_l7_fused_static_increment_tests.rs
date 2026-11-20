//! Interpreter round i2 wave 67, lane L7: the fused static int increment
//! (`CRATONVM_INTERP_FUSED_STATIC_INCREMENT`, `fused_static_increment`).
//! A hit stores what the unfused `getstatic; <operand>; iadd | isub;
//! putstatic` stores (the wrapping int result, at the site's holder and
//! slot) and leaves the operand stack as it found it; every decline
//! writes nothing.
use super::fused_static_increment;
use crate::runtime::interpreter::site_cache::PinnedSites;

/// The static site table every test here runs against: entries a test
/// fills are pinned, so another test thread moving the process epochs
/// between the fill and the helper's lookup cannot turn an expected hit
/// into a cold-site decline (wave 73, lane L7; page
/// `i70-orch-fused-increment-unit-tests-fail-intermittently-in-the-full-suite-20261103`).
type PinnedStatic = PinnedSites<ResolvedField>;
use crate::classloading::resolution::ResolvedField;
use crate::classloading::ClassId;
use crate::runtime::frame::Frame;
use crate::types::Value;

/// A static `(I)V` frame over `code` (local 0 = `5`, `max_locals` 2).
fn frame(class_id: ClassId, code: Vec<u8>) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/i67l7/Increment".to_string(),
        "m".to_string(),
        "(I)V".to_string(),
        None,
        code,
        vec![],
        4,
        2,
        &[Value::Int(5)],
    )
}

/// `getstatic #0x0102; <operand>; <arith>; putstatic #0x0102; return`.
fn group(operand: &[u8], arith: u8) -> Vec<u8> {
    let mut code = vec![0xb2, 0x01, 0x02];
    code.extend_from_slice(operand);
    code.extend_from_slice(&[arith, 0xb3, 0x01, 0x02, 0xb1]);
    code
}

fn fill_static(
    sites: &mut PinnedStatic,
    class_id: ClassId,
    holder: ClassId,
    desc: u8,
    volatile: bool,
    is_final: bool,
) -> bool {
    sites.pin(
        class_id,
        0x0102,
        ResolvedField {
            declaring_class_id: holder,
            field_index: 3,
            is_static: true,
            is_volatile: volatile,
            is_final,
            is_reference: matches!(desc, b'L' | b'['),
            desc_byte: desc,
        },
    )
}

/// Run the fusion at pc 0 of `f` over a field holding `current`; the
/// next pc and what was stored.
fn run(
    f: &Frame,
    sites: &mut PinnedStatic,
    current: Value,
) -> (Option<usize>, Option<(ClassId, usize, Value)>) {
    run_wide(f, sites, current, false)
}

/// [`run`] with `CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE` as `wide`
/// (wave 68, lane L7).
fn run_wide(
    f: &Frame,
    sites: &mut PinnedStatic,
    current: Value,
    wide: bool,
) -> (Option<usize>, Option<(ClassId, usize, Value)>) {
    run_mode(f, sites, current, super::static_increment_mode(wide, false))
}

/// [`run`] with the entry's whole `mode` byte (wave 69, lane L7: the
/// early-decline screen is [`super::STATIC_INC_EARLY_DECLINE`]).
fn run_mode(
    f: &Frame,
    sites: &mut PinnedStatic,
    current: Value,
    mode: u8,
) -> (Option<usize>, Option<(ClassId, usize, Value)>) {
    let code_len = f.code.len() - 2;
    let mut stored = None;
    let next = sites.run(|cache| {
        stored = None;
        // Since wave 83 (lane L7) the helper takes the thread's grouped
        // tables: `cache` is moved in for the call and back after it.
        let mut tables = crate::runtime::interpreter::InterpSiteTables::new();
        std::mem::swap(&mut tables.static_field_sites, cache);
        let next = fused_static_increment(
            f,
            code_len,
            0,
            mode,
            &mut tables,
            |_, _| current,
            |c, slot, v| {
                stored = Some((c, slot, v));
            },
        );
        std::mem::swap(&mut tables.static_field_sites, cache);
        next
    });
    (next, stored)
}

#[test]
fn an_increment_stores_what_the_unfused_group_stores() {
    let class_id = ClassId::new(0x00ab_0671);
    let holder = ClassId::new(0x00ab_0672);
    // (operand bytes, the int it pushes)
    let operands: [(&[u8], i32); 9] = [
        (&[0x02], -1),
        (&[0x03], 0),
        (&[0x04], 1),
        (&[0x08], 5),
        (&[0x10, 0x80], -128),
        (&[0x10, 0x7f], 127),
        (&[0x11, 0x80, 0x00], -32768),
        (&[0x15, 0x00], 5),
        (&[0x1a], 5),
    ];
    for (operand, k) in operands {
        for (arith, subtract) in [(0x60_u8, false), (0x64_u8, true)] {
            for volatile in [false, true] {
                for current in [Value::Int(10), Value::Int(i32::MAX), Value::Object(None)] {
                    let mut sites = PinnedStatic::new();
                    if !fill_static(&mut sites, class_id, holder, b'I', volatile, false) {
                        continue;
                    }
                    let code = group(operand, arith);
                    let end = code.len() - 1;
                    let mut f = frame(class_id, code);
                    f.stack.push_int(42).unwrap();
                    let (next, stored) = run(&f, &mut sites, current);
                    let base = match current {
                        Value::Int(x) => x,
                        _ => 0,
                    };
                    let want = if subtract {
                        base.wrapping_sub(k)
                    } else {
                        base.wrapping_add(k)
                    };
                    assert_eq!(
                        next,
                        Some(end),
                        "lands on the `return` after the `putstatic`"
                    );
                    let (c, slot, v) = stored.expect("a hit stores");
                    assert_eq!((c, slot), (holder, 3));
                    assert!(
                        matches!(v, Value::Int(x) if x == want),
                        "{operand:?} {arith:#x}"
                    );
                    assert_eq!(f.stack.len(), 1, "nothing pushed or popped");
                    assert_eq!(f.stack.pop_int().ok(), Some(42));
                }
            }
        }
    }
}

#[test]
fn every_other_shape_declines_without_writing() {
    let class_id = ClassId::new(0x00ab_0673);
    let holder = ClassId::new(0x00ab_0674);
    // A miss: the unfused arms resolve and fill.
    let f = frame(class_id, group(&[0x04], 0x60));
    let mut empty = PinnedStatic::new();
    let (next, stored) = run(&f, &mut empty, Value::Int(1));
    assert!(next.is_none() && stored.is_none());
    let declines = |code: Vec<u8>, desc: u8, is_final: bool, current: Value| {
        let mut sites = PinnedStatic::new();
        if !fill_static(&mut sites, class_id, holder, desc, false, is_final) {
            return;
        }
        let f = frame(class_id, code);
        let (next, stored) = run(&f, &mut sites, current);
        assert!(next.is_none() && stored.is_none());
    };
    // Not `I`: a sub-int needs its narrowing store, a long is two slots.
    for desc in *b"SBCZJFL" {
        declines(group(&[0x04], 0x60), desc, false, Value::Int(1));
    }
    // A final static: the quickened `putstatic` declines it too.
    declines(group(&[0x04], 0x60), b'I', true, Value::Int(1));
    // A value shape the push would not take as an int.
    declines(group(&[0x04], 0x60), b'I', false, Value::Long(1));
    // Another arithmetic opcode (`imul`, `iand`).
    declines(group(&[0x04], 0x68), b'I', false, Value::Int(1));
    declines(group(&[0x04], 0x7e), b'I', false, Value::Int(1));
    // Another operand (`ldc`, `aload_0`), a local past `max_locals`.
    declines(group(&[0x12, 0x01], 0x60), b'I', false, Value::Int(1));
    declines(group(&[0x2a], 0x60), b'I', false, Value::Int(1));
    declines(group(&[0x15, 0xff], 0x60), b'I', false, Value::Int(1));
    // A `putstatic` of another pool index, or a `getstatic` instead.
    declines(
        vec![0xb2, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x03, 0xb1],
        b'I',
        false,
        Value::Int(1),
    );
    declines(
        vec![0xb2, 0x01, 0x02, 0x04, 0x60, 0xb2, 0x01, 0x02, 0xb1],
        b'I',
        false,
        Value::Int(1),
    );
    // The group cut by the end of the code (the padding is never read
    // as its tail).
    declines(
        vec![0xb2, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01],
        b'I',
        false,
        Value::Int(1),
    );
    declines(
        vec![0xb2, 0x01, 0x02, 0x04, 0x60],
        b'I',
        false,
        Value::Int(1),
    );
}

#[test]
fn a_non_int_local_declines() {
    let class_id = ClassId::new(0x00ab_0675);
    let holder = ClassId::new(0x00ab_0676);
    let mut sites = PinnedStatic::new();
    if !fill_static(&mut sites, class_id, holder, b'I', false, false) {
        return;
    }
    let f = Frame::new(
        class_id,
        "cratonvm/test/i67l7/Increment".to_string(),
        "f".to_string(),
        "(F)V".to_string(),
        None,
        group(&[0x1a], 0x60),
        vec![],
        4,
        1,
        &[Value::Float(1.5)],
    );
    let (next, stored) = run(&f, &mut sites, Value::Int(1));
    assert!(next.is_none() && stored.is_none());
}

// ── Wave 68, lane L7: `CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE` ──

/// `getstatic #0x0102; <operand>; <arith>; <tail...>; putstatic #0x0102;
/// return`.
fn wide_group(operand: &[u8], arith: u8, tail: &[u8]) -> Vec<u8> {
    let mut code = vec![0xb2, 0x01, 0x02];
    code.extend_from_slice(operand);
    code.push(arith);
    code.extend_from_slice(tail);
    code.extend_from_slice(&[0xb3, 0x01, 0x02, 0xb1]);
    code
}

#[test]
fn a_narrow_increment_stores_the_narrowed_int() {
    let class_id = ClassId::new(0x00ab_0681);
    let holder = ClassId::new(0x00ab_0682);
    // (descriptor, conversion, current, arith, want)
    let cases: [(u8, u8, i32, u8, i32); 6] = [
        (b'B', 0x91, 127, 0x60, -128),
        (b'B', 0x91, -128, 0x64, 127),
        (b'C', 0x92, 0, 0x64, 65535),
        (b'C', 0x92, 65535, 0x60, 0),
        (b'S', 0x93, 32767, 0x60, -32768),
        (b'S', 0x93, 5, 0x64, 4),
    ];
    for (desc, conv, current, arith, want) in cases {
        let mut sites = PinnedStatic::new();
        if !fill_static(&mut sites, class_id, holder, desc, false, false) {
            continue;
        }
        let code = wide_group(&[0x04], arith, &[conv]);
        let end = code.len() - 1;
        let f = frame(class_id, code);
        // Off: the wave-67 rule (an `I` field, no conversion) declines.
        let (next, stored) = run_wide(&f, &mut sites, Value::Int(current), false);
        assert!(next.is_none() && stored.is_none());
        let (next, stored) = run_wide(&f, &mut sites, Value::Int(current), true);
        assert_eq!(next, Some(end), "{} {conv:#x}", desc as char);
        let (c, slot, v) = stored.expect("a hit stores");
        assert_eq!((c, slot), (holder, 3));
        assert!(
            matches!(v, Value::Int(x) if x == want),
            "{} {conv:#x} {current} {arith:#x}: {v:?}",
            desc as char
        );
        assert_eq!(super::narrow_increment_result(want, Some(conv)), want);
    }
}

#[test]
fn a_conversion_not_the_fields_own_declines() {
    let class_id = ClassId::new(0x00ab_0683);
    let holder = ClassId::new(0x00ab_0684);
    // (descriptor, conversion)
    for (desc, conv) in [
        (b'B', 0x93_u8),
        (b'S', 0x91),
        (b'C', 0x93),
        (b'I', 0x91),
        (b'Z', 0x91),
        (b'J', 0x91),
    ] {
        let mut sites = PinnedStatic::new();
        if !fill_static(&mut sites, class_id, holder, desc, false, false) {
            continue;
        }
        let f = frame(class_id, wide_group(&[0x04], 0x60, &[conv]));
        let (next, stored) = run_wide(&f, &mut sites, Value::Int(1), true);
        assert!(
            next.is_none() && stored.is_none(),
            "{} {conv:#x}",
            desc as char
        );
    }
    // A narrow field without its conversion (`b += 1` always has one).
    let mut sites = PinnedStatic::new();
    if fill_static(&mut sites, class_id, holder, b'B', false, false) {
        let f = frame(class_id, wide_group(&[0x04], 0x60, &[]));
        let (next, stored) = run_wide(&f, &mut sites, Value::Int(1), true);
        assert!(next.is_none() && stored.is_none());
    }
    // A final narrow static.
    let mut sites = PinnedStatic::new();
    if fill_static(&mut sites, class_id, holder, b'B', false, true) {
        let f = frame(class_id, wide_group(&[0x04], 0x60, &[0x91]));
        let (next, stored) = run_wide(&f, &mut sites, Value::Int(1), true);
        assert!(next.is_none() && stored.is_none());
    }
}

#[test]
fn a_long_increment_stores_the_long() {
    let class_id = ClassId::new(0x00ab_0685);
    let holder = ClassId::new(0x00ab_0686);
    // (operand, arith, current, want)
    let cases: [(u8, u8, Value, i64); 6] = [
        (0x0a, 0x61, Value::Long(41), 42),
        (0x0a, 0x61, Value::Long(i64::MAX), i64::MIN),
        (0x0a, 0x65, Value::Long(0), -1),
        (0x09, 0x61, Value::Long(7), 7),
        (0x0a, 0x61, Value::Int(5), 6),
        (0x0a, 0x61, Value::Object(None), 1),
    ];
    for (operand, arith, current, want) in cases {
        for volatile in [false, true] {
            let mut sites = PinnedStatic::new();
            if !fill_static(&mut sites, class_id, holder, b'J', volatile, false) {
                continue;
            }
            let code = wide_group(&[operand], arith, &[]);
            let end = code.len() - 1;
            let mut f = frame(class_id, code);
            f.stack.push_int(42).unwrap();
            let (next, stored) = run_wide(&f, &mut sites, current, false);
            assert!(next.is_none() && stored.is_none(), "off declines");
            let (next, stored) = run_wide(&f, &mut sites, current, true);
            assert_eq!(next, Some(end));
            let (c, slot, v) = stored.expect("a hit stores");
            assert_eq!((c, slot), (holder, 3));
            assert!(
                matches!(v, Value::Long(x) if x == want),
                "{current:?}: {v:?}"
            );
            assert_eq!(f.stack.len(), 1, "nothing pushed or popped");
        }
    }
    // A value shape the quickened `getstatic` would not push as a long.
    let mut sites = PinnedStatic::new();
    if fill_static(&mut sites, class_id, holder, b'J', false, false) {
        let f = frame(class_id, wide_group(&[0x0a], 0x61, &[]));
        let (next, stored) = run_wide(&f, &mut sites, Value::Double(1.0), true);
        assert!(next.is_none() && stored.is_none());
        // An int arithmetic opcode after a long operand, a narrowing after
        // a long add.
        let f = frame(class_id, wide_group(&[0x0a], 0x60, &[]));
        let (next, stored) = run_wide(&f, &mut sites, Value::Long(1), true);
        assert!(next.is_none() && stored.is_none());
        let f = frame(class_id, wide_group(&[0x0a], 0x61, &[0x91]));
        let (next, stored) = run_wide(&f, &mut sites, Value::Long(1), true);
        assert!(next.is_none() && stored.is_none());
    }
    // A long group over an `I` field.
    let mut sites = PinnedStatic::new();
    if fill_static(&mut sites, class_id, holder, b'I', false, false) {
        let f = frame(class_id, wide_group(&[0x0a], 0x61, &[]));
        let (next, stored) = run_wide(&f, &mut sites, Value::Int(1), true);
        assert!(next.is_none() && stored.is_none());
    }
}

#[test]
fn a_long_local_operand_needs_its_long_mark() {
    let class_id = ClassId::new(0x00ab_0687);
    let holder = ClassId::new(0x00ab_0688);
    let mut sites = PinnedStatic::new();
    if !fill_static(&mut sites, class_id, holder, b'J', false, false) {
        return;
    }
    let long_frame = |code: Vec<u8>| {
        Frame::new(
            class_id,
            "cratonvm/test/i68l7/Increment".to_string(),
            "l".to_string(),
            "(J)V".to_string(),
            None,
            code,
            vec![],
            6,
            3,
            &[Value::Long(1_000_000_007)],
        )
    };
    for operand in [&[0x1e_u8][..], &[0x16, 0x00][..]] {
        let f = long_frame(wide_group(operand, 0x61, &[]));
        let (next, stored) = run_wide(&f, &mut sites, Value::Long(3), true);
        if f.local_is_long_marked(0) {
            assert!(next.is_some());
            let (_, _, v) = stored.expect("a hit stores");
            assert!(matches!(v, Value::Long(1_000_000_010)), "{v:?}");
        } else {
            assert!(next.is_none() && stored.is_none());
        }
    }
    // An int local, and a long local past `max_locals`.
    let f = frame(class_id, wide_group(&[0x1e], 0x61, &[]));
    let (next, stored) = run_wide(&f, &mut sites, Value::Long(3), true);
    assert!(next.is_none() && stored.is_none());
    let f = long_frame(wide_group(&[0x16, 0x02], 0x61, &[]));
    let (next, stored) = run_wide(&f, &mut sites, Value::Long(3), true);
    assert!(next.is_none() && stored.is_none());
}

// ── Wave 69, lane L7: `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE` ──

/// Groups the screen must agree with the body on: every operand form,
/// both widths, conversions, and the shapes that decline on bytes alone.
fn screen_corpus() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let operands: [&[u8]; 14] = [
        &[0x02],
        &[0x04],
        &[0x08],
        &[0x10, 0x80],
        &[0x11, 0x03, 0xe8],
        &[0x15, 0x00],
        &[0x15, 0x09],
        &[0x1a],
        &[0x1b],
        &[0x09],
        &[0x0a],
        &[0x16, 0x00],
        &[0x1e],
        &[0x12, 0x01],
    ];
    for operand in operands {
        for arith in [0x60_u8, 0x64, 0x61, 0x65, 0x68] {
            for tail in [&[][..], &[0x91][..], &[0x92][..], &[0x93][..], &[0x88][..]] {
                out.push(wide_group(operand, arith, tail));
            }
        }
    }
    // `x = y + 1`: another `putstatic` index; a `putfield`; cut short.
    out.push(vec![0xb2, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x03, 0xb1]);
    out.push(vec![0xb2, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1]);
    out.push(vec![0xb2, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01]);
    out.push(vec![0xb2, 0x01, 0x02, 0x04]);
    out
}

#[test]
fn the_early_decline_screen_changes_no_answer() {
    let class_id = ClassId::new(0x00ab_0691);
    let holder = ClassId::new(0x00ab_0692);
    for desc in *b"IBCSJ" {
        let mut sites = PinnedStatic::new();
        if !fill_static(&mut sites, class_id, holder, desc, false, false) {
            continue;
        }
        let current = if desc == b'J' {
            Value::Long(41)
        } else {
            Value::Int(41)
        };
        for code in screen_corpus() {
            let f = frame(class_id, code.clone());
            for wide in [false, true] {
                let mode = super::static_increment_mode(wide, false);
                let early = super::static_increment_mode(wide, true);
                let plain = run_mode(&f, &mut sites, current, mode);
                let screened = run_mode(&f, &mut sites, current, early);
                assert_eq!(plain, screened, "{} wide={wide} {code:x?}", desc as char);
                // A hit is always a group the screen passed.
                if plain.0.is_some() {
                    let code_len = f.code.len() - 2;
                    assert!(super::static_increment_screen(
                        &f.code, code_len, 0, wide, false
                    ));
                }
            }
        }
    }
}

#[test]
fn the_screen_refuses_on_bytes_alone() {
    let screen = |code: &[u8], wide: bool| {
        let f = frame(ClassId::new(0x00ab_0693), code.to_vec());
        let code_len = f.code.len() - 2;
        super::static_increment_screen(&f.code, code_len, 0, wide, false)
    };
    assert!(screen(&group(&[0x04], 0x60), false));
    assert!(
        screen(&group(&[0x15, 0x09], 0x64), false),
        "the local is not read"
    );
    // `x = y + 1` (the wave-67 `pairBody` row).
    assert!(!screen(
        &[0xb2, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x03, 0xb1],
        true
    ));
    // A long operand and a conversion only under `wide`.
    assert!(!screen(&wide_group(&[0x0a], 0x61, &[]), false));
    assert!(screen(&wide_group(&[0x0a], 0x61, &[]), true));
    assert!(!screen(&wide_group(&[0x04], 0x60, &[0x91]), false));
    assert!(screen(&wide_group(&[0x04], 0x60, &[0x91]), true));
    // A conversion after a long operation, `ldc`, `imul`.
    assert!(!screen(&wide_group(&[0x0a], 0x61, &[0x91]), true));
    assert!(!screen(&group(&[0x12, 0x01], 0x60), true));
    assert!(!screen(&group(&[0x04], 0x68), true));
    // Cut by the end of the code.
    assert!(!screen(&[0xb2, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01], true));
}

// ── Interpreter round i3 wave 83, lane L7:
// `CRATONVM_INTERP_FUSED_INCREMENT_LDC2W` (`STATIC_INC_LDC2W`) ──

use crate::runtime::interpreter::site_cache::{NameEpochs, PrimitiveConstant};

/// The thread's numeric-constant table, pinned as `PinnedStatic` is.
type PinnedConsts = PinnedSites<PrimitiveConstant, NameEpochs>;

/// `getstatic #0x0102; ldc2_w #0x0007; <arith>; putstatic #0x0102;
/// return`.
fn ldc2w_group(arith: u8) -> Vec<u8> {
    group(&[0x14, 0x00, 0x07], arith)
}

/// [`run_mode`] over both tables: the two pinned tables are moved into
/// one `InterpSiteTables` for the call and back after it.
fn run_ldc2w(
    f: &Frame,
    sites: &mut PinnedStatic,
    consts: &mut PinnedConsts,
    current: Value,
    mode: u8,
) -> (Option<usize>, Option<(ClassId, usize, Value)>) {
    let code_len = f.code.len() - 2;
    let mut stored = None;
    let next = consts.run(|ccache| {
        sites.run(|scache| {
            stored = None;
            let mut tables = crate::runtime::interpreter::InterpSiteTables::new();
            std::mem::swap(&mut tables.static_field_sites, scache);
            std::mem::swap(&mut tables.prim_const_sites, ccache);
            let next = fused_static_increment(
                f,
                code_len,
                0,
                mode,
                &mut tables,
                |_, _| current,
                |c, slot, v| {
                    stored = Some((c, slot, v));
                },
            );
            std::mem::swap(&mut tables.static_field_sites, scache);
            std::mem::swap(&mut tables.prim_const_sites, ccache);
            next
        })
    });
    (next, stored)
}

#[test]
fn an_ldc2w_long_operand_is_added_only_under_its_bit_and_wide() {
    let class_id = ClassId::new(0x00ab_0887);
    let holder = ClassId::new(0x00ab_0888);
    let wide = super::STATIC_INC_WIDE;
    let ldc2w = super::STATIC_INC_LDC2W;
    let early = super::STATIC_INC_EARLY_DECLINE;
    // (arith, constant, current, want): wrapping, as `ladd` / `lsub`.
    let cases: [(u8, i64, Value, i64); 4] = [
        (0x61, 1_000_000, Value::Long(5), 1_000_005),
        (0x65, 1024, Value::Long(0), -1024),
        (0x61, 1, Value::Long(i64::MAX), i64::MIN),
        (0x61, 8, Value::Object(None), 8),
    ];
    for (arith, k, current, want) in cases {
        let mut sites = PinnedStatic::new();
        if !fill_static(&mut sites, class_id, holder, b'J', false, false) {
            return;
        }
        let mut consts = PinnedConsts::new();
        if !consts.pin(class_id, 0x0007, PrimitiveConstant::Long(k)) {
            return;
        }
        let f = frame(class_id, ldc2w_group(arith));
        let end = f.code.len() - 2 - 1;
        for off in [0, wide, wide | early, ldc2w, ldc2w | early] {
            assert_eq!(
                run_ldc2w(&f, &mut sites, &mut consts, current, off),
                (None, None),
                "mode {off}"
            );
        }
        for on in [wide | ldc2w, wide | ldc2w | early] {
            assert_eq!(
                run_ldc2w(&f, &mut sites, &mut consts, current, on),
                (Some(end), Some((holder, 3, Value::Long(want)))),
                "mode {on}, k {k}"
            );
        }
    }
}

#[test]
fn an_ldc2w_miss_a_double_or_an_int_field_declines() {
    let class_id = ClassId::new(0x00ab_0889);
    let holder = ClassId::new(0x00ab_088a);
    let on = super::STATIC_INC_WIDE | super::STATIC_INC_LDC2W;
    // A `Double` entry, an `Int` one, and a miss (another index).
    for (constant, index) in [
        (PrimitiveConstant::Double(2.0), 0x0007),
        (PrimitiveConstant::Int(3), 0x0007),
        (PrimitiveConstant::Long(3), 0x0008),
    ] {
        let mut sites = PinnedStatic::new();
        let mut consts = PinnedConsts::new();
        if !fill_static(&mut sites, class_id, holder, b'J', false, false)
            || !consts.pin(class_id, index, constant)
        {
            return;
        }
        let f = frame(class_id, ldc2w_group(0x61));
        assert_eq!(
            run_ldc2w(&f, &mut sites, &mut consts, Value::Long(1), on),
            (None, None)
        );
    }
    // A long constant on an `I` field.
    let mut sites = PinnedStatic::new();
    let mut consts = PinnedConsts::new();
    if fill_static(&mut sites, class_id, holder, b'I', false, false)
        && consts.pin(class_id, 0x0007, PrimitiveConstant::Long(1))
    {
        let f = frame(class_id, ldc2w_group(0x61));
        assert_eq!(
            run_ldc2w(&f, &mut sites, &mut consts, Value::Int(1), on),
            (None, None)
        );
    }
}

#[test]
fn the_ldc2w_operand_reader_and_the_screen() {
    let class_id = ClassId::new(0x00ab_088b);
    let code = ldc2w_group(0x61);
    let mut consts = super::site_cache::PrimitiveConstantSiteCache::new();
    for _ in 0..100 {
        let epochs = super::site_cache::PrimitiveConstantSiteCache::epochs_now();
        consts.put(class_id, 0x0007, epochs, PrimitiveConstant::Long(-9));
        let found = super::ldc2w_long_operand(&code, code.len(), 3, class_id, &mut consts);
        if let Some(found) = found {
            assert_eq!(found, (-9, 6));
            break;
        }
    }
    // Not an `ldc2_w`, or cut off inside its index: `None`.
    assert_eq!(
        super::ldc2w_long_operand(&code, code.len(), 0, class_id, &mut consts),
        None
    );
    assert_eq!(
        super::ldc2w_long_operand(&code, 5, 3, class_id, &mut consts),
        None
    );
    // Another class's frame: `None`.
    let other = ClassId::new(0x00ab_088c);
    assert_eq!(
        super::ldc2w_long_operand(&code, code.len(), 3, other, &mut consts),
        None
    );
    // The screen passes the group only under `wide` and `ldc2w`.
    let f = frame(class_id, code);
    let code_len = f.code.len() - 2;
    assert!(super::static_increment_screen(
        &f.code, code_len, 0, true, true
    ));
    assert!(!super::static_increment_screen(
        &f.code, code_len, 0, true, false
    ));
    assert!(!super::static_increment_screen(
        &f.code, code_len, 0, false, true
    ));
}
