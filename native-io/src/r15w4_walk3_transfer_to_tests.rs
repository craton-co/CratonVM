use super::*;
use crate::test_support::MockNativeContext;

/// `Objects.requireNonNull(out, "out")` before the first read.
#[test]
fn a_null_out_throws_npe_out_before_reading() {
    if !transfer_to_exact() {
        return; // `CRATONVM_COMPAT_TRANSFER_TO_EXACT=0` in the environment
    }
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(1);
    let result =
        native_is_transfer_to(&mut ctx, &[Value::Object(Some(this)), Value::Object(None)]);
    assert!(
        matches!(
            result,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { message: Some(ref m) }
            ))) if m == "out"
        ),
        "got {result:?}"
    );
    assert!(ctx.recorded_calls().is_empty(), "nothing was read");
}

/// A `read` that returns `0` is written as an empty chunk and the loop
/// reads again, so the bytes after it arrive and are counted.
#[test]
fn a_zero_length_read_is_written_and_the_loop_goes_on() {
    if !transfer_to_exact() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(1);
    let out = ctx.alloc_object(1);
    ctx.script("read", "([BII)I", Ok(Some(Value::Int(0))));
    ctx.script("read", "([BII)I", Ok(Some(Value::Int(3))));
    ctx.script("read", "([BII)I", Ok(Some(Value::Int(-1))));
    let result = native_is_transfer_to(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(out))],
    )
    .expect("transferTo");
    assert_eq!(result, Some(Value::Long(3)));
    let shape: Vec<(String, Option<Value>)> = ctx
        .recorded_calls()
        .iter()
        .map(|c| (c.method_name.clone(), c.args.get(2).copied()))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("read".to_string(), Some(Value::Int(16 * 1024))),
            ("write".to_string(), Some(Value::Int(0))),
            ("read".to_string(), Some(Value::Int(16 * 1024))),
            ("write".to_string(), Some(Value::Int(3))),
            ("read".to_string(), Some(Value::Int(16 * 1024))),
        ]
    );
}
