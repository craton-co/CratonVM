use super::*;


#[test]
fn i125_l6_the_yield_record_precedes_the_question_mark() {
    let src = include_str!("../lambda.rs");
    let record = ["record_lambda_global_impl_owner_on_yield(", "shared, &call_site);"].concat();
    let question = ["let r = ", "r?;"].concat();
    let at_record = src.find(&record).expect("the yield arm records the owner");
    let at_question = src[at_record..].find(&question).map(|i| i + at_record);
    assert!(at_question.is_some(), "the `?` follows the yield arm's record");
}

/// JIT round 23 wave 2, lane L3 (`CRATONVM_LAMBDA_OWNER_MEMO_ON_THROW`):
/// a THROWING by-name static dispatch records the owner too, through the
/// initialized-class rule, before the `?` that leaves -- otherwise a SAM
/// recursion that ends in StackOverflowError never reaches the compiled
/// direct arm.
#[test]
fn r23_w2_a_throwing_dispatch_records_the_owner_before_leaving() {
    let src = include_str!("../lambda.rs");
    let gate = "if r.is_err() {";
    let at_gate = src.find(&gate).expect("the throw arm is gated");
    let record = [
        "record_lambda_global_impl_owner_on_yield(",
        "shared, &call_site);",
    ]
    .concat();
    let at_record = src[at_gate..]
        .find(&record)
        .map(|i| i + at_gate)
        .expect("records");
    let question = ["let r = ", "r?;"].concat();
    let at_question = src[at_record..].find(&question).map(|i| i + at_record);
    assert!(at_question.is_some(), "the `?` follows the throw arm's record");
    // The record itself refuses a class that is not initialized.
    let body = src
        .split("fn record_lambda_global_impl_owner_on_yield(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the record body");
    assert!(body.contains("is_class_initialized_via_manager(shared, cid)"));
}

/// JIT round 23 wave 5, lane L6 (`CRATONVM_LAMBDA_OWNER_MEMO_BEFORE_CALL`):
/// the by-name static arm files the owner, through the initialized-class
/// rule, BEFORE `invoke_static_shared` -- so a SAM recursion's first chain
/// already reaches the memoised / compiled direct arm at its second level.
#[test]
fn r23_w5_the_owner_is_filed_before_the_by_name_dispatch() {
    let src = include_str!("../lambda.rs");
    let gate = [
        "if lambda_owner_memo_before_call_enabled(",
        "shared) {",
    ]
    .concat();
    let at_gate = src.find(&gate).expect("the pre-call record is gated");
    let record = [
        "record_lambda_global_impl_owner_on_yield(",
        "shared, &call_site);",
    ]
    .concat();
    let at_record = src[at_gate..]
        .find(&record)
        .map(|i| i + at_gate)
        .expect("records");
    let call = ["let r = crate::vm::", "invoke_static_shared("].concat();
    let at_call = src[at_record..]
        .find(&call)
        .map(|i| i + at_record)
        .expect("the dispatch follows the record");
    // Nothing else sits between the record and the dispatch's line.
    assert!(!src[at_record + record.len()..at_call].contains("return"));
}

