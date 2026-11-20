//! STRGAP19-1: the Latin-1 fast path renders what the general renderer
//! renders (HotSpot's `String.valueOf` spellings), declines everything it
//! does not own, and builds a LATIN1 String of exactly the result length.

use super::*;

fn units(s: &str) -> Arc<[u16]> {
    Arc::from(s.encode_utf16().collect::<Vec<u16>>().as_slice())
}

fn render(recipe: &str, constants: &[&str], types: &str, values: &[Value]) -> Option<String> {
    let recipe: Vec<u16> = recipe.encode_utf16().collect();
    let constants: Vec<Arc<[u16]>> = constants.iter().map(|c| units(c)).collect();
    let types: Vec<char> = types.chars().collect();
    let mut out = Latin1Buf::new();
    render_latin1(
        &recipe,
        &constants,
        &types,
        values,
        |_, _| false, // no heap in these tests: any live reference declines
        &mut out,
    )
    .then(|| out.iter().map(|&b| b as char).collect())
}

#[test]
fn primitives_render_as_string_value_of_spells_them() {
    let got = render(
        "z=\x01 b=\x01 s=\x01 i=\x01 j=\x01 c=\x01 n=\x01",
        &[],
        "ZBSIJCL",
        &[
            Value::Int(1),
            Value::Int(-128),
            Value::Int(-32768),
            Value::Int(i32::MIN),
            Value::Long(i64::MIN),
            Value::Int(0xE9), // 'é', Latin-1
            Value::Object(None),
        ],
    );
    assert_eq!(
        got.as_deref(),
        Some("z=true b=-128 s=-32768 i=-2147483648 j=-9223372036854775808 c=\u{e9} n=null")
    );
    assert_eq!(
        render(
            "\x01|\x01|\x01",
            &[],
            "ZIJ",
            &[Value::Int(0), Value::Int(0), Value::Long(i64::MAX)]
        )
        .as_deref(),
        Some("false|0|9223372036854775807")
    );
}

#[test]
fn floats_use_the_general_renderers_formatters() {
    let got = render(
        "\x01;\x01",
        &[],
        "FD",
        &[Value::Float(1.0e10), Value::Double(-0.0)],
    );
    let want = format!(
        "{};{}",
        crate::runtime::invokedynamic::format_float(1.0e10),
        crate::runtime::invokedynamic::format_double(-0.0)
    );
    assert_eq!(got.as_deref(), Some(want.as_str()));
}

#[test]
fn constants_are_copied_in_recipe_order() {
    let got = render("<\x02|\x01|\x02>", &["K", "Q"], "I", &[Value::Int(7)]);
    assert_eq!(got.as_deref(), Some("<K|7|Q>"));
}

#[test]
fn what_needs_utf16_or_tostring_declines() {
    // A `char` above 0xFF makes the result UTF16.
    assert_eq!(render("\x01", &[], "C", &[Value::Int(0x3A9)]), None);
    // A live reference this reader does not accept (a non-String, or a
    // UTF16 String) declines: its `toString()` belongs to the general path.
    // SAFETY: never dereferenced; the test reader declines every reference.
    let fake = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
    assert_eq!(render("\x01", &[], "L", &[Value::Object(Some(fake))]), None);
    // Operands that disagree with the recipe decline.
    assert_eq!(render("\x01\x01", &[], "I", &[Value::Int(1)]), None);
    assert_eq!(render("\x01", &[], "I", &[Value::Long(1)]), None);
}


/// End to end on a heap: a `String` operand, a `null`, an `int` and a
/// constant; the result is a fresh LATIN1 String of exactly the result's
/// length, and a UTF16 String operand or compact strings off decline.
#[test]
fn a_latin1_result_is_one_exact_size_string() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::{create_java_string_uninterned, read_java_string};
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "strgap19-latin1");
    let recipe: Vec<u16> = "v=\x01/\x01/\x01\x02".encode_utf16().collect();
    let constants = [units("!")];
    let types = ['I', 'L', 'L'];

    // Compact strings off (the test VM's default): declines.
    let s = create_java_string_uninterned(&shared, "s\u{e9}");
    let values = [Value::Int(-42), Value::Object(Some(s)), Value::Object(None)];
    assert!(try_concat(&shared, &mut thread, &recipe, &constants, &types, &values).is_none());

    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    // Compact strings over the synthetic two-slot String layout (no real
    // image produces it): the constructor refuses rather than write the
    // `hash` / `hashIsZero` slots past the object, so even an operand-free
    // render declines.
    let primitive_only = [Value::Int(1), Value::Object(None), Value::Object(None)];
    assert!(try_concat(
        &shared,
        &mut thread,
        &recipe,
        &constants,
        &types,
        &primitive_only
    )
    .is_none());

    // The JDK 9+ layout (`value`, `coder`, `hash`, `hashIsZero`), as a real
    // image resolves it.
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);
    let s = create_java_string_uninterned(&shared, "s\u{e9}");
    let values = [Value::Int(-42), Value::Object(Some(s)), Value::Object(None)];
    let got = try_concat(&shared, &mut thread, &recipe, &constants, &types, &values)
        .expect("an all-Latin-1 concat takes the fast path");
    assert_eq!(
        read_java_string(&shared.mem.heap, got).as_deref(),
        Some("v=-42/s\u{e9}/null!")
    );
    let mut bytes = Latin1Buf::new();
    assert!(crate::vm::append_java_string_latin1(
        &shared.mem.heap,
        got,
        &mut bytes
    ));
    assert_eq!(bytes.len(), "v=-42/s\u{e9}/null!".chars().count());
    assert_ne!(got.as_ptr(), s.as_ptr());

    // A UTF16 String operand declines (the general renderer answers it).
    let wide = create_java_string_uninterned(&shared, "\u{3a9}");
    let values = [
        Value::Int(1),
        Value::Object(Some(wide)),
        Value::Object(None),
    ];
    assert!(try_concat(&shared, &mut thread, &recipe, &constants, &types, &values).is_none());
}

/// STRGAP19-7: the interpreter's entry renders exactly what the general
/// renderer renders, a fresh String per call, and declines a recipe whose
/// tags disagree with the operands. (The unary boolean `"" + b` declines:
/// see [`the_unary_boolean_concat_answers_the_interned_literal`].)
#[test]
fn the_interpreted_entry_matches_the_general_renderer() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::{create_java_string_uninterned, read_java_string};
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "strgap19-7-interp");
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);
    let s = create_java_string_uninterned(&shared, "ab\u{e9}");
    let cases: Vec<(&str, Vec<Arc<[u16]>>, Vec<char>, Vec<Value>)> = vec![
        ("b=\x01", vec![], vec!['Z'], vec![Value::Int(1)]),
        (
            "\x01\x01",
            vec![],
            vec!['Z', 'Z'],
            vec![Value::Int(0), Value::Int(1)],
        ),
        (
            "k=\x01,\x01;\x02",
            vec![units("end")],
            vec!['J', 'L'],
            vec![Value::Long(i64::MIN), Value::Object(Some(s))],
        ),
        (
            "\x01\x01\x01",
            vec![],
            vec!['C', 'D', 'L'],
            vec![Value::Int(0xE9), Value::Double(-0.0), Value::Object(None)],
        ),
    ];
    for (recipe_text, constants, types, values) in &cases {
        let recipe: Vec<u16> = recipe_text.encode_utf16().collect();
        let fast = try_concat_interpreted(
            &shared,
            &mut thread,
            &recipe,
            constants,
            types,
            values,
            None,
        )
        .expect("an all-Latin-1 interpreted concat takes the fast path");
        let general = super::super::concat_values_to_string(
            &shared,
            &mut thread,
            &recipe,
            constants.as_slice(),
            types,
            values,
            false,
        )
        .expect("the general renderer answers the same concat");
        assert_eq!(
            read_java_string(&shared.mem.heap, fast),
            read_java_string(&shared.mem.heap, general),
            "recipe {recipe_text:?}"
        );
        assert_ne!(fast.as_ptr(), general.as_ptr(), "a fresh String each");
    }
    // Tags that disagree with the operands: the general renderer's
    // tolerance is not reproduced; this entry declines.
    let recipe: Vec<u16> = "\x01\x01".encode_utf16().collect();
    assert!(try_concat_interpreted(
        &shared,
        &mut thread,
        &recipe,
        &[] as &[Arc<[u16]>],
        &['I'],
        &[Value::Int(3)],
        None,
    )
    .is_none());
    // INTERP719-1: a cached `false` declines without the scan.
    let recipe: Vec<u16> = "\x01".encode_utf16().collect();
    assert!(try_concat_interpreted(
        &shared,
        &mut thread,
        &recipe,
        &[] as &[Arc<[u16]>],
        &['I'],
        &[Value::Int(3)],
        Some(false),
    )
    .is_none());
}

/// `r19-unary-indy-concat-returns-a-fresh-string-20261008` (wave 11, lane
/// hoist11): `"" + b` -- recipe `\1`, no constants, one `Z` -- is the
/// interned `"true"` / `"false"` literal (HotSpot's
/// `String.valueOf(boolean)`), the same object every call; both fast
/// paths decline it so the general renderer answers. `"" + i` and a
/// boolean with literal text stay fresh Strings.
#[test]
fn the_unary_boolean_concat_answers_the_interned_literal() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::read_java_string;
    if !crate::runtime::env_cache::concat_unary_boolean_literal() {
        return; // killed by CRATONVM_CONCAT_UNARY_BOOLEAN_LITERAL=0
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "hoist11-unary-z");
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);
    let recipe: Vec<u16> = "\x01".encode_utf16().collect();
    let none: &[Arc<[u16]>] = &[];
    for (v, text) in [(1, "true"), (0, "false"), (7, "true")] {
        let values = [Value::Int(v)];
        assert!(try_concat(&shared, &mut thread, &recipe, none, &['Z'], &values).is_none());
        assert!(try_concat_interpreted(
            &shared,
            &mut thread,
            &recipe,
            none,
            &['Z'],
            &values,
            None
        )
        .is_none());
        let render = |thread: &mut JvmThread| {
            super::super::concat_values_to_string(
                &shared,
                thread,
                &recipe,
                none,
                &['Z'],
                &values,
                false,
            )
            .expect("the general renderer answers")
        };
        let a = render(&mut thread);
        let b = render(&mut thread);
        let (pooled, _) = crate::vm::try_create_java_string_reporting(&shared, text)
            .expect("the literal pool answers");
        assert_eq!(read_java_string(&shared.mem.heap, a).as_deref(), Some(text));
        assert_eq!(a.as_ptr(), b.as_ptr(), "the same object every call");
        assert_eq!(a.as_ptr(), pooled.as_ptr(), "the interned literal");
    }
    // `"" + i` stays a fresh String (HotSpot: `Integer.toString`).
    let values = [Value::Int(1)];
    let a = super::super::concat_values_to_string(
        &shared,
        &mut thread,
        &recipe,
        none,
        &['I'],
        &values,
        false,
    )
    .expect("int");
    let b = super::super::concat_values_to_string(
        &shared,
        &mut thread,
        &recipe,
        none,
        &['I'],
        &values,
        false,
    )
    .expect("int");
    assert_ne!(a.as_ptr(), b.as_ptr());
}

/// INTERP719-1: the cached site's answer, parsed from the descriptor once,
/// is the per-call scan's answer (the scan over the argument types
/// `execute_string_concat` parses from the same descriptor) for every
/// shape the rule distinguishes.
#[test]
fn the_cached_shape_is_the_per_call_scans_answer() {
    let cases: Vec<(&str, Vec<&str>, &str)> = vec![
        (
            "v=\x01/\x01",
            vec![],
            "(ILjava/lang/String;)Ljava/lang/String;",
        ),
        ("\x01", vec![], "(Z)Ljava/lang/String;"),
        ("k=\x01,\x01;\x02", vec!["end"], "(J[I)Ljava/lang/String;"),
        ("\u{3a9}=\x01", vec![], "(I)Ljava/lang/String;"),
        ("\x02\x01", vec!["\u{3a9}"], "(I)Ljava/lang/String;"),
        ("\x01\x01", vec![], "(I)Ljava/lang/String;"),
        ("\x01", vec!["extra"], "(I)Ljava/lang/String;"),
        ("\x01\x01\x01", vec![], "(CDF)Ljava/lang/String;"),
        ("", vec![], "()Ljava/lang/String;"),
    ];
    let mut admitted = 0;
    for (recipe_text, constants, descriptor) in &cases {
        let recipe: Vec<u16> = recipe_text.encode_utf16().collect();
        let constants: Vec<Arc<[u16]>> = constants.iter().map(|c| units(c)).collect();
        let mut types: smallvec::SmallVec<[char; 8]> = smallvec::SmallVec::new();
        super::super::parse_descriptor_args_into(descriptor, &mut types);
        let scanned = site_shape_admits_in(&recipe, &constants, &types);
        assert_eq!(
            site_shape_admits_for_descriptor(&recipe, &constants, descriptor),
            scanned,
            "recipe {recipe_text:?} descriptor {descriptor}"
        );
        admitted += usize::from(scanned);
    }
    // Positive and negative controls: both answers occur.
    assert!(
        admitted >= 3 && admitted < cases.len(),
        "admitted {admitted}"
    );
}

/// r21 concat (`CRATONVM_CONCAT_LENGTH_OVERFLOW_OOME`): the bounds are
/// `StringConcatHelper`'s -- a LATIN1 result up to `Integer.MAX_VALUE`
/// units, a UTF16 one up to 2^30 - 1 (`length << 1` stays an `int`), and
/// compact strings off makes every result UTF16. The wide scan is asked
/// only past the UTF16 bound. The message is HotSpot's, text for text.
#[test]
fn r21_concat_length_bounds_are_string_concat_helpers() {
    use super::super::{
        concat_length_overflows, concat_length_overflows_by, CONCAT_LENGTH_OVERFLOW_MESSAGE,
        CONCAT_MAX_LATIN1_UNITS, CONCAT_MAX_UTF16_UNITS,
    };
    assert_eq!(CONCAT_MAX_LATIN1_UNITS, 2_147_483_647);
    assert_eq!(CONCAT_MAX_UTF16_UNITS, (1 << 30) - 1);
    let never = || -> bool { panic!("the wide scan below the UTF16 bound") };
    assert!(!concat_length_overflows_by(0, false, never));
    assert!(!concat_length_overflows_by(CONCAT_MAX_UTF16_UNITS, true, never));
    // LATIN1 past 2^30 - 1 is fine; UTF16 is not.
    assert!(!concat_length_overflows_by(1 << 30, false, || false));
    assert!(concat_length_overflows_by(1 << 30, false, || true));
    assert!(concat_length_overflows_by(1 << 30, true, || false));
    assert!(!concat_length_overflows_by(CONCAT_MAX_LATIN1_UNITS, false, || false));
    assert!(concat_length_overflows_by(CONCAT_MAX_LATIN1_UNITS + 1, false, || false));
    // The slice form reads the units it is given.
    assert!(!concat_length_overflows(&[0x41, 0x3A9], false));
    assert_eq!(CONCAT_LENGTH_OVERFLOW_MESSAGE, "Overflow: String length out of range");
    let mut out = Latin1Buf::new();
    out.extend_from_slice(b"ab");
    assert!(out.len() <= CONCAT_MAX_LATIN1_UNITS);
}


/// CONCAT21 review (cat21): under `-XX:-CompactStrings` every String is
/// UTF16 (`COMPACT_STRINGS` false, the layout bit still true), so the UTF16
/// fast path builds a UTF16 String even when every unit fits a byte, where
/// it used to build LATIN1 (read back by the JDK as UTF16 units). The
/// default policy still builds LATIN1 for the same units. An operand that
/// would pass the bound is not copied.
#[test]
fn r21_cat21_the_utf16_fast_path_honours_compact_strings_off() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::{create_java_string_uninterned, read_java_string_units};
    let recipe: Vec<u16> = "<\x01|\x01|\x01>".encode_utf16().collect();
    let none: &[Arc<[u16]>] = &[];
    let types = ['L', 'C', 'I'];
    let values = [Value::Object(None), Value::Int(0xE9), Value::Int(1)];
    let want: Vec<u16> = "<null|\u{e9}|1>".encode_utf16().collect();
    for (jvm_args, latin1) in [
        (vec!["-XX:-CompactStrings".to_string()], false),
        (Vec::new(), true),
    ] {
        let mut config = crate::config::VmConfig::default();
        config.jvm_args = jvm_args;
        let shared = SharedVm::new(config);
        let mut thread = JvmThread::new(ThreadId(0), "r21-cat21-utf16-off");
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(shared.classes.latin1_coder_allowed(), latin1);
        let got = try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values)
            .expect("primitives and null take the UTF16 path");
        let mut bytes = Latin1Buf::new();
        assert_eq!(
            crate::vm::append_java_string_latin1(&shared.mem.heap, got, &mut bytes),
            latin1,
            "LATIN1 only while LATIN1 Strings may exist"
        );
        assert_eq!(
            read_java_string_units(&shared.mem.heap, got),
            Some(want.clone())
        );
        let s = create_java_string_uninterned(&shared, "ab");
        assert!(!string_would_pass_the_bound(&shared, s, 0));
        assert!(!string_would_pass_the_bound(
            &shared,
            s,
            super::super::CONCAT_MAX_LATIN1_UNITS - 2
        ));
        assert!(string_would_pass_the_bound(
            &shared,
            s,
            super::super::CONCAT_MAX_LATIN1_UNITS - 1
        ));
    }
}

/// CONCAT21-2: a result with a unit above 0xFF is one UTF16 String from the
/// TLAB with the general renderer's text; one whose units all fit a byte
/// is LATIN1; compact strings off declines.
#[test]
fn r21_a_utf16_result_is_one_tlab_string() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::{create_java_string_uninterned, read_java_string_units};
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "r21-concat-utf16");
    let recipe: Vec<u16> = "<\x01|\x01|\x01>".encode_utf16().collect();
    let none: &[Arc<[u16]>] = &[];
    let types = ['L', 'C', 'I'];
    let values = [Value::Object(None), Value::Int(0x3A9), Value::Int(5)];
    // Compact strings off (the test VM's default): declines.
    assert!(try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values).is_none());
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);
    let got = try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values)
        .expect("primitives and null take the UTF16 path");
    let want: Vec<u16> = "<null|\u{3a9}|5>".encode_utf16().collect();
    assert_eq!(read_java_string_units(&shared.mem.heap, got), Some(want));
    // A UTF16 String operand is copied unit for unit.
    let w = create_java_string_uninterned(&shared, "\u{4e2d}\u{e9}");
    let types = ['L', 'C', 'I'];
    let values = [Value::Object(Some(w)), Value::Int(u32::from(b'c') as i32), Value::Int(-1)];
    let got = try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values)
        .expect("a UTF16 String operand");
    let want: Vec<u16> = "<\u{4e2d}\u{e9}|c|-1>".encode_utf16().collect();
    assert_eq!(read_java_string_units(&shared.mem.heap, got), Some(want));
    // All units fit a byte: a LATIN1 String (the general renderer's coder).
    let values = [Value::Object(None), Value::Int(0xE9), Value::Int(1)];
    let got = try_concat_utf16(&shared, &mut thread, &recipe, none, &types, &values)
        .expect("latin1 units");
    let mut bytes = Latin1Buf::new();
    assert!(crate::vm::append_java_string_latin1(&shared.mem.heap, got, &mut bytes));
    assert_eq!(&bytes[..], b"<null|\xe9|1>");
    // `"" + b` keeps the interned literal (the general renderer).
    let unary: Vec<u16> = "\x01".encode_utf16().collect();
    if crate::runtime::env_cache::concat_unary_boolean_literal() {
        assert!(
            try_concat_utf16(&shared, &mut thread, &unary, none, &['Z'], &[Value::Int(1)])
                .is_none()
        );
    }
}
