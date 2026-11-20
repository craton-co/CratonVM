use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn sp_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_p60_flow(&mut r);
    register_p69_submission_publisher(&mut r);
    r
}

#[test]
fn b6_submission_publisher_core_methods_registered() {
    let r = sp_registry();
    let sp = "java/util/concurrent/SubmissionPublisher";
    assert!(r.find(sp, "submit", "(Ljava/lang/Object;)I").is_some());
    assert!(r
        .find(sp, "subscribe", "(Ljava/util/concurrent/Flow$Subscriber;)V")
        .is_some());
    assert!(r.find(sp, "hasSubscribers", "()Z").is_some());
    assert!(r.find(sp, "getNumberOfSubscribers", "()I").is_some());
}

#[test]
fn b6_subscribe_grows_subscriber_list_and_counts() {
    let r = sp_registry();
    let mut ctx = mock_ctx();
    let pub_cid = ctx
        .ensure_class_initialized("java/util/concurrent/SubmissionPublisher")
        .unwrap();
    let publisher = ctx.alloc_object(pub_cid, 3);
    // Fresh publisher: no subscribers.
    ctx.set_field(publisher, 0, Value::Object(None));
    ctx.set_field(publisher, 1, Value::Int(0));

    let count = r
        .find(
            "java/util/concurrent/SubmissionPublisher",
            "getNumberOfSubscribers",
            "()I",
        )
        .unwrap();
    let n0 = count(&mut ctx, &[Value::Object(Some(publisher))])
        .unwrap()
        .unwrap();
    assert_eq!(n0, Value::Int(0), "fresh publisher has 0 subscribers");

    // Register two distinct subscribers.
    let sub_cid = ctx.ensure_class_initialized("FlowSubscriberImpl").unwrap();
    let s1 = ctx.alloc_object(sub_cid, 1);
    let s2 = ctx.alloc_object(sub_cid, 1);
    let subscribe = r
        .find(
            "java/util/concurrent/SubmissionPublisher",
            "subscribe",
            "(Ljava/util/concurrent/Flow$Subscriber;)V",
        )
        .unwrap();
    subscribe(
        &mut ctx,
        &[Value::Object(Some(publisher)), Value::Object(Some(s1))],
    )
    .unwrap();
    subscribe(
        &mut ctx,
        &[Value::Object(Some(publisher)), Value::Object(Some(s2))],
    )
    .unwrap();

    let n2 = count(&mut ctx, &[Value::Object(Some(publisher))])
        .unwrap()
        .unwrap();
    assert_eq!(n2, Value::Int(2), "two subscribers registered");

    let has = r
        .find(
            "java/util/concurrent/SubmissionPublisher",
            "hasSubscribers",
            "()Z",
        )
        .unwrap();
    assert_eq!(
        has(&mut ctx, &[Value::Object(Some(publisher))])
            .unwrap()
            .unwrap(),
        Value::Int(1),
        "hasSubscribers true once registered"
    );

    // field 0 is an ArrayList-style wrapper; its field 0 is the backing
    // array and field 1 is the count. Both subscribers are stored in order.
    let wrapper = match ctx.get_field(publisher, 0) {
        Value::Object(Some(w)) => w,
        other => panic!("subscribers field should be a wrapper object, got {other:?}"),
    };
    assert_eq!(
        ctx.get_field(wrapper, 1),
        Value::Int(2),
        "wrapper count is 2"
    );
    if let Value::Object(Some(arr)) = ctx.get_field(wrapper, 0) {
        assert!(ctx.array_length(arr) >= 2);
        assert_eq!(ctx.get_array_element(arr, 0), Value::Object(Some(s1)));
        assert_eq!(ctx.get_array_element(arr, 1), Value::Object(Some(s2)));
    } else {
        panic!("wrapper field 0 should be a reference array after subscribe");
    }
}

#[test]
fn b6_subscribe_null_subscriber_throws_npe() {
    let r = sp_registry();
    let mut ctx = mock_ctx();
    let pub_cid = ctx
        .ensure_class_initialized("java/util/concurrent/SubmissionPublisher")
        .unwrap();
    let publisher = ctx.alloc_object(pub_cid, 3);
    let subscribe = r
        .find(
            "java/util/concurrent/SubmissionPublisher",
            "subscribe",
            "(Ljava/util/concurrent/Flow$Subscriber;)V",
        )
        .unwrap();
    let res = subscribe(
        &mut ctx,
        &[Value::Object(Some(publisher)), Value::Object(None)],
    );
    assert!(
        res.is_err(),
        "null subscriber must throw, not silently no-op"
    );
}

#[test]
fn b6_submit_to_closed_publisher_throws() {
    let r = sp_registry();
    let mut ctx = mock_ctx();
    let pub_cid = ctx
        .ensure_class_initialized("java/util/concurrent/SubmissionPublisher")
        .unwrap();
    let publisher = ctx.alloc_object(pub_cid, 3);
    ctx.set_field(publisher, 0, Value::Object(None));
    ctx.set_field(publisher, 1, Value::Int(1)); // closed
    let submit = r
        .find(
            "java/util/concurrent/SubmissionPublisher",
            "submit",
            "(Ljava/lang/Object;)I",
        )
        .unwrap();
    let item = ctx.create_string("x");
    let res = submit(
        &mut ctx,
        &[Value::Object(Some(publisher)), Value::Object(Some(item))],
    );
    assert!(
        res.is_err(),
        "submit on a closed publisher must throw, not return a fake lag"
    );
}

#[test]
fn b6_submit_with_no_subscribers_returns_zero_lag() {
    let r = sp_registry();
    let mut ctx = mock_ctx();
    let pub_cid = ctx
        .ensure_class_initialized("java/util/concurrent/SubmissionPublisher")
        .unwrap();
    let publisher = ctx.alloc_object(pub_cid, 3);
    ctx.set_field(publisher, 0, Value::Object(None));
    ctx.set_field(publisher, 1, Value::Int(0));
    let submit = r
        .find(
            "java/util/concurrent/SubmissionPublisher",
            "submit",
            "(Ljava/lang/Object;)I",
        )
        .unwrap();
    let item = ctx.create_string("x");
    // No subscribers -> 0 delivered (the old code returned a hardcoded 1).
    let lag = submit(
        &mut ctx,
        &[Value::Object(Some(publisher)), Value::Object(Some(item))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        lag,
        Value::Int(0),
        "no subscribers => estimated lag 0, not a fake 1"
    );
}

fn charset_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_phase55_charset(&mut r);
    r
}

#[test]
fn b7_charset_for_name_rejects_unknown() {
    let r = charset_registry();
    let mut ctx = mock_ctx();
    let for_name = r
        .find(
            "java/nio/charset/Charset",
            "forName",
            "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        )
        .unwrap();
    let bogus = ctx.create_string("NOT-A-REAL-CHARSET-9000");
    let res = for_name(&mut ctx, &[Value::Object(Some(bogus))]);
    assert!(
        res.is_err(),
        "forName on an unknown charset must throw (IAE supertype of UnsupportedCharsetException), not return a fake Charset"
    );
}

#[test]
fn b7_charset_for_name_accepts_known() {
    let r = charset_registry();
    let mut ctx = mock_ctx();
    let for_name = r
        .find(
            "java/nio/charset/Charset",
            "forName",
            "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        )
        .unwrap();
    let name = ctx.create_string("utf-8");
    let res = for_name(&mut ctx, &[Value::Object(Some(name))])
        .expect("forName(utf-8) ok")
        .expect("forName(utf-8) returns a Charset");
    match res {
        Value::Object(Some(cs)) => {
            // Canonical name normalises to "UTF-8".
            match ctx.get_field(cs, 0) {
                Value::Object(Some(s)) => {
                    assert_eq!(ctx.read_string(s).as_deref(), Some("UTF-8"));
                }
                other => panic!("charset name field should be a String, got {other:?}"),
            }
        }
        other => panic!("forName should return a Charset object, got {other:?}"),
    }
}
