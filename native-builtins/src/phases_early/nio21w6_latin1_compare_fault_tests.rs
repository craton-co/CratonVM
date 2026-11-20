use super::{latin1_compare_to, latin1_element_fault, Latin1LoadFault as F};

fn cmp(a: Option<&[u8]>, b: Option<&[u8]>, len1: i32, len2: i32) -> Result<i32, F> {
    let a_len = a.map(|x| x.len() as i32);
    let b_len = b.map(|x| x.len() as i32);
    latin1_compare_to(a_len, b_len, len1, len2, |first, i| {
        let arr = if first { a } else { b };
        i32::from(arr.expect("screened")[i as usize])
    })
}

/// Each row is HotSpot 25.0.3's answer, measured through reflection.
#[test]
fn r21w6_compare_to_answers_as_the_bytecode() {
    assert_eq!(cmp(None, Some(&[0]), 1, 1), Err(F::Null("a")));
    assert_eq!(cmp(Some(&[0, 0]), None, 1, 1), Err(F::Null("b")));
    // `lim <= 0` touches neither array.
    assert_eq!(cmp(None, None, 0, 3), Ok(-3));
    assert_eq!(cmp(None, None, -4, 2), Ok(-6));
    assert_eq!(cmp(Some(&[0, 0]), Some(&[0, 0]), 3, 3), Err(F::Range(2, 2)));
    assert_eq!(cmp(None, Some(&[0; 9]), 9, 9), Err(F::Null("a")));
    // Unsigned bytes; the first difference wins.
    assert_eq!(cmp(Some(&[1, 0xff]), Some(&[1, 1]), 2, 2), Ok(0xff - 1));
    assert_eq!(cmp(Some(b"ab"), Some(b"abc"), 2, 3), Ok(-1));
    // A difference before the out-of-range index is the answer.
    assert_eq!(cmp(Some(&[1]), Some(&[2]), 4, 4), Ok(-1));
    // `b`'s load faults after `a[i]` loaded.
    assert_eq!(cmp(Some(&[0, 0, 0]), Some(&[0]), 3, 3), Err(F::Range(1, 1)));
}

#[test]
fn r21w6_get_char_faults_as_baload() {
    assert_eq!(latin1_element_fault(None, 0, "val"), Some(F::Null("val")));
    assert_eq!(
        latin1_element_fault(Some(3), 5, "val"),
        Some(F::Range(5, 3))
    );
    assert_eq!(
        latin1_element_fault(Some(3), -1, "val"),
        Some(F::Range(-1, 3))
    );
    assert_eq!(latin1_element_fault(Some(3), 2, "val"), None);
    let msg = |f: F| match f.into_error() {
        cratonvm_types::error::RuntimeError::NullPointerException { message } => message,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        msg(F::Null("val")).as_deref(),
        Some("Cannot load from byte/boolean array because \"val\" is null")
    );
}

/// r21 (2026-10-13), through the registry: `getChar` has no native (the
/// JDK body runs), the two `compareTo`s and `inflate` are leaves, an
/// in-range call is answered natively, and every fault re-enters the JDK
/// body (the mock's `invoke_hook` answers with a sentinel and records the
/// call), so the exception is thrown from HotSpot's frames.
#[test]
fn r21_registered_latin1_natives_hand_faults_to_the_jdk_body() {
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{NativeHeapAccess, NativeMethodRegistry};
    use cratonvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
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
    const C: &str = "java/lang/StringLatin1";
    let mut r = NativeMethodRegistry::new();
    super::register_string_latin1_natives(&mut r);
    assert!(r.find(C, "getChar", "([BI)C").is_none());
    let leaves = r.leaf_registrations();
    for (m, d) in [
        ("compareTo", "([B[B)I"),
        ("compareTo", "([B[BII)I"),
        ("inflate", "([BI[CII)V"),
    ] {
        assert!(leaves.contains(&(C, m, d)), "{m}{d} is not a leaf");
    }
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_invoke_hook(body);
    let ab = ctx.new_array(ArrayElementType::Byte, 2);
    let ac = ctx.new_array(ArrayElementType::Byte, 2);
    ctx.set_array_element(ab, 0, Value::Int(b'a' as i32));
    ctx.set_array_element(ab, 1, Value::Int(b'b' as i32));
    ctx.set_array_element(ac, 0, Value::Int(b'a' as i32));
    ctx.set_array_element(ac, 1, Value::Int(b'c' as i32));
    let b2 = ctx.new_array(ArrayElementType::Byte, 2);
    let o = |x| Value::Object(Some(x));
    let null = Value::Object(None);
    let int = Value::Int;
    let mut call = |m: &str, desc: &str, args: &[Value]| -> String {
        let f = r.find(C, m, desc).expect("registered");
        match f(&mut ctx, args) {
            Ok(v) => format!("{v:?}"),
            Err(MethodCallFailed::InternalError(VmError::Runtime(e))) => format!("{e:?}"),
            Err(other) => panic!("{other:?}"),
        }
    };
    let ok = |v| format!("{:?}", Some(Value::Int(v)));
    let body_ran = ok(4242);
    // In range, or touching no element: native answers.
    assert_eq!(call("compareTo", "([B[B)I", &[o(ab), o(ac)]), ok(-1));
    assert_eq!(
        call("compareTo", "([B[BII)I", &[null, null, int(0), int(3)]),
        ok(-3)
    );
    assert_eq!(
        call("compareTo", "([B[BII)I", &[o(ab), o(ac), int(2), int(2)]),
        ok(-1)
    );
    assert_eq!(
        call(
            "inflate",
            "([BI[CII)V",
            &[null, int(0), null, int(0), int(0)]
        ),
        "None"
    );
    BODY_CALLS.with(|c| assert!(c.borrow().is_empty()));
    // Faults: the JDK body throws them.
    assert_eq!(call("compareTo", "([B[B)I", &[null, o(b2)]), body_ran);
    assert_eq!(call("compareTo", "([B[B)I", &[o(b2), null]), body_ran);
    assert_eq!(
        call("compareTo", "([B[BII)I", &[null, o(b2), int(1), int(1)]),
        body_ran
    );
    assert_eq!(
        call("compareTo", "([B[BII)I", &[o(b2), o(b2), int(3), int(3)]),
        body_ran
    );
    assert_eq!(
        call(
            "inflate",
            "([BI[CII)V",
            &[null, int(0), null, int(0), int(1)]
        ),
        body_ran
    );
    BODY_CALLS.with(|c| {
        assert_eq!(
            *c.borrow(),
            [
                "compareTo([B[B)I",
                "compareTo([B[B)I",
                "compareTo([B[BII)I",
                "compareTo([B[BII)I",
                "inflate([BI[CII)V"
            ]
            .map(|m| format!("{C}.{m}"))
        );
    });
}
