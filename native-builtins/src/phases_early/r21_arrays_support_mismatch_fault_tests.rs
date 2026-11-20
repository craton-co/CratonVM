use super::{arrays_mismatch_fault_error, arrays_mismatch_plan, Latin1LoadFault as F};

/// The JDK body's answer over `a` / `b` (`None`: null), composed as
/// `arrays_support_mismatch_answer` does.
fn mm(
    a: Option<&[u16]>,
    a_from: i32,
    b: Option<&[u16]>,
    b_from: i32,
    length: i32,
    hotspot: bool,
) -> Result<i32, F> {
    let len = |x: Option<&[u16]>| x.map(|s| s.len() as i32);
    let p = arrays_mismatch_plan(len(a), a_from, len(b), b_from, length, hotspot);
    for i in 0..p.n {
        let va = a.expect("planned")[(p.a_from + i) as usize];
        let vb = b.expect("planned")[(p.b_from + i) as usize];
        if va != vb {
            return Ok(i);
        }
    }
    p.fault.map_or(Ok(-1), Err)
}

fn hs(a: Option<&[u16]>, af: i32, b: Option<&[u16]>, bf: i32, length: i32) -> Result<i32, F> {
    mm(a, af, b, bf, length, true)
}

/// Each row is HotSpot 25.0.3's answer (`ArraysSupport.mismatch` called
/// directly under `--add-exports java.base/jdk.internal.util`).
#[test]
fn r21_mismatch_answers_as_the_jdk_body() {
    let (z1, z2, z9, z0): (&[u16], &[u16], &[u16], &[u16]) = (&[0], &[0, 0], &[0; 9], &[]);
    let z2b: &[u16] = &[0, 1];
    assert_eq!(hs(None, 0, Some(z1), 0, 1), Err(F::Null("a")));
    assert_eq!(hs(Some(z1), 0, None, 0, 1), Err(F::Null("b")));
    assert_eq!(hs(None, 0, Some(z9), 0, 9), Err(F::Null("a")));
    assert_eq!(hs(Some(z9), 0, None, 0, 9), Err(F::Null("b")));
    assert_eq!(hs(None, 0, None, 0, 0), Ok(-1));
    assert_eq!(hs(None, 0, None, 0, -3), Ok(-1));
    assert_eq!(hs(Some(z2), 0, Some(z2), 0, 3), Err(F::Range(2, 2)));
    assert_eq!(hs(Some(z2), 0, Some(z1), 0, 2), Err(F::Range(1, 1)));
    assert_eq!(hs(Some(z1), 0, Some(z2), 0, 2), Err(F::Range(1, 1)));
    // A difference before the out-of-range index is the answer.
    assert_eq!(hs(Some(z2), 0, Some(z2b), 0, 3), Ok(1));
    assert_eq!(hs(Some(z0), 0, Some(z9), 0, 9), Err(F::Range(0, 0)));
    assert_eq!(hs(Some(z9), 0, Some(z0), 0, 9), Err(F::Range(0, 0)));
    // The offset overloads.
    assert_eq!(hs(None, 0, None, 0, -1), Ok(-1));
    assert_eq!(hs(Some(z2), 1, Some(z2), 0, 2), Err(F::Range(2, 2)));
    assert_eq!(hs(Some(z2), 0, Some(z2), 1, 2), Err(F::Range(2, 2)));
    assert_eq!(hs(Some(z2), -1, Some(z2), 0, 1), Err(F::Range(-1, 2)));
    assert_eq!(hs(Some(z2), 0, Some(z2), -2, 1), Err(F::Range(-2, 2)));
    assert_eq!(hs(Some(z2), 5, Some(z2), 0, 1), Err(F::Range(5, 2)));
    assert_eq!(hs(Some(z2), 1, Some(z2b), 1, 2), Ok(0));
    assert_eq!(hs(Some(z9), 9, Some(z9), 0, 9), Err(F::Range(9, 9)));
    // In range: the first difference, else `-1`.
    assert_eq!(hs(Some(&[1, 2, 3]), 0, Some(&[1, 2, 4]), 0, 3), Ok(2));
    assert_eq!(hs(Some(&[9, 1, 2, 3]), 1, Some(&[1, 2, 5]), 0, 3), Ok(2));
    assert_eq!(hs(Some(&[9, 1, 2]), 1, Some(&[1, 2]), 0, 2), Ok(-1));
    // `from + i` never wraps before the array's end faults.
    assert_eq!(hs(Some(z2), 1, Some(z2), 1, i32::MAX), Err(F::Range(2, 2)));
}

/// The `byte[]` `mismatch` overloads are `Intrinsic`s only in a VM without a
/// JIT; with one the compiled JDK body wins (`startsWith("jar:")` 205 -> 24
/// ns, one binary). The `char[]` overloads (ECJ) register in both. The
/// `arrays-mismatch-byte` row of `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`
/// brings the byte rows back in a JIT VM, marked body-compilable.
#[test]
fn mismatch_byte_natives_register_without_a_jit_only() {
    use cratonvm_native_api::NativeMethodRegistry;
    let c = "jdk/internal/util/ArraysSupport";
    let rows = [
        ("([B[BI)I", true),
        ("([BI[BII)I", true),
        ("([C[CI)I", false),
        ("([CI[CII)I", false),
    ];
    let mut jit = NativeMethodRegistry::new();
    jit.set_jit_enabled(true);
    jit.set_intrinsic_body_compiles_rows("");
    super::register_arrays_support_natives(&mut jit);
    let mut nojit = NativeMethodRegistry::new();
    nojit.set_jit_enabled(false);
    super::register_arrays_support_natives(&mut nojit);
    let mut row = NativeMethodRegistry::new();
    row.set_jit_enabled(true);
    row.set_intrinsic_body_compiles_rows("arrays-mismatch-byte");
    super::register_arrays_support_natives(&mut row);
    for (desc, byte) in rows {
        assert_eq!(jit.find(c, "mismatch", desc).is_some(), !byte, "{desc} with a JIT");
        assert!(nojit.find(c, "mismatch", desc).is_some(), "{desc} without a JIT");
        assert!(row.find(c, "mismatch", desc).is_some(), "{desc} with the row");
        assert_eq!(
            row.is_body_compilable_intrinsic(c, "mismatch", desc),
            byte,
            "{desc} body-compilable mark"
        );
    }
}

#[test]
fn r21_mismatch_legacy_answers_behind_the_switch() {
    let z2: &[u16] = &[0, 0];
    assert_eq!(mm(None, 0, Some(z2), 0, 1, false), Ok(-1));
    assert_eq!(mm(Some(z2), 0, None, 0, 1, false), Ok(-1));
    assert_eq!(mm(Some(z2), 0, Some(z2), 0, 3, false), Ok(-1));
    assert_eq!(mm(Some(z2), -1, Some(&[0, 1]), 0, 2, false), Ok(1));
    assert_eq!(mm(Some(z2), 5, Some(z2), 0, 1, false), Ok(-1));
}

#[test]
fn r21_mismatch_npe_text_names_the_element_type() {
    let msg = |e| match e {
        cratonvm_types::error::RuntimeError::NullPointerException { message } => message,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        msg(arrays_mismatch_fault_error(F::Null("a"), 1)).as_deref(),
        Some("Cannot load from byte/boolean array because \"a\" is null")
    );
    assert_eq!(
        msg(arrays_mismatch_fault_error(F::Null("b"), 2)).as_deref(),
        Some("Cannot load from char array because \"b\" is null")
    );
}

/// Through the registered natives: each overload's answer, its own error
/// where `vectorizedMismatch` would read past an array, and a re-entry
/// into the JDK body for every other fault (the mock's `invoke_hook`
/// answers it with a sentinel and records the descriptor). Compared by
/// `Debug`; `RuntimeError` has no `PartialEq`.
#[test]
fn r21_registered_mismatch_natives_hand_faults_to_the_jdk_body() {
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{NativeHeapAccess, NativeMethodRegistry};
    use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
    use cratonvm_types::{ArrayElementType, Value};
    use std::cell::RefCell;
    thread_local! {
        static BODY_CALLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }
    fn body(
        _: &mut MockNativeContext,
        class: &str,
        name: &str,
        desc: &str,
        _: &[Value],
    ) -> Option<MethodCallResult> {
        BODY_CALLS.with(|c| c.borrow_mut().push(format!("{class}.{name}{desc}")));
        Some(Ok(Some(Value::Int(4242))))
    }
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_invoke_hook(body);
    let mut r = NativeMethodRegistry::new();
    // The byte[] overloads register only without a JIT (`interpreted_intrinsic_registers_row`).
    r.set_jit_enabled(false);
    super::register_arrays_support_natives(&mut r);
    let b2 = ctx.new_array(ArrayElementType::Byte, 2);
    let b9 = ctx.new_array(ArrayElementType::Byte, 9);
    let c2 = ctx.new_array(ArrayElementType::Char, 2);
    let c2b = ctx.new_array(ArrayElementType::Char, 2);
    let c4 = ctx.new_array(ArrayElementType::Char, 4);
    ctx.set_array_element(c2b, 1, Value::Int(7));
    let o = |x| Value::Object(Some(x));
    let null = Value::Object(None);
    let int = Value::Int;
    let mut call = |desc: &str, args: &[Value]| -> String {
        let f = r
            .find("jdk/internal/util/ArraysSupport", "mismatch", desc)
            .expect("registered");
        match f(&mut ctx, args) {
            Ok(v) => format!("{v:?}"),
            Err(MethodCallFailed::InternalError(VmError::Runtime(e))) => format!("{e:?}"),
            Err(other) => panic!("{other:?}"),
        }
    };
    let range = |i, n| format!("{:?}", RuntimeError::aioobe(i, n));
    let ok = |v| format!("{:?}", Some(Value::Int(v)));
    let body_ran = ok(4242);
    // In range, or touching no element: native answers.
    assert_eq!(call("([B[BI)I", &[null, null, int(0)]), ok(-1));
    assert_eq!(call("([C[CI)I", &[o(c2), o(c2b), int(3)]), ok(1));
    assert_eq!(
        call("([CI[CII)I", &[o(c2), int(1), o(c2b), int(1), int(1)]),
        ok(0)
    );
    assert_eq!(
        call("([BI[BII)I", &[o(b9), int(0), o(b9), int(0), int(9)]),
        ok(-1)
    );
    BODY_CALLS.with(|c| assert!(c.borrow().is_empty()));
    // Faults the JDK body raises: re-entered.
    assert_eq!(call("([B[BI)I", &[null, o(b2), int(1)]), body_ran);
    assert_eq!(call("([B[BI)I", &[o(b2), o(b2), int(3)]), body_ran);
    assert_eq!(call("([C[CI)I", &[o(c2), null, int(1)]), body_ran);
    assert_eq!(
        call("([BI[BII)I", &[o(b2), int(-1), o(b2), int(0), int(1)]),
        body_ran
    );
    assert_eq!(
        call("([CI[CII)I", &[o(c2), int(0), o(c2), int(1), int(2)]),
        body_ran
    );
    // The prelude's index-0 load past `length > 7`.
    assert_eq!(
        call("([BI[BII)I", &[o(b9), int(9), o(b9), int(0), int(9)]),
        body_ran
    );
    BODY_CALLS.with(|c| {
        assert_eq!(
            *c.borrow(),
            [
                "([B[BI)I",
                "([B[BI)I",
                "([C[CI)I",
                "([BI[BII)I",
                "([CI[CII)I",
                "([BI[BII)I"
            ]
            .map(|d| format!("jdk/internal/util/ArraysSupport.mismatch{d}"))
        );
        c.borrow_mut().clear();
    });
    // Past an array after a clean index 0 with `length > 7` (`> 3` for
    // `char[]`): undefined on HotSpot; the native answers the loop's AIOOBE.
    assert_eq!(call("([B[BI)I", &[o(b9), o(b9), int(10)]), range(9, 9));
    assert_eq!(call("([C[CI)I", &[o(c4), o(c4), int(5)]), range(4, 4));
    assert_eq!(
        call("([CI[CII)I", &[o(c4), int(1), o(c4), int(0), int(4)]),
        range(4, 4)
    );
    BODY_CALLS.with(|c| assert!(c.borrow().is_empty()));
}
