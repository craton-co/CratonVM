use super::{
    jit_site_access_admit, jit_site_access_admitted, JitInvokeInfo, JIT_SITE_ACCESS_LAST,
    JIT_SITE_ACCESS_PASSED,
};

/// The text of `sig`'s item, up to its closing brace at column 0.
fn item(sig: &str) -> String {
    let whole = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = whole.find(sig);
    assert!(start.is_some(), "`{sig}` not found");
    let body = &whole[start.unwrap_or(0)..];
    body[..body.find("\n}\n").map_or(body.len(), |e| e + 3)].to_string()
}

/// Failed before t1 wave 2 (lane P): a compiled `invokestatic` of an owner
/// no loader had loaded at compile time reached the callee through these
/// helpers without the JVMS §5.4.4 check the interpreter runs after the
/// load (`T1PRuntimePackageAccess` `AccJit.freshCls`). The guard must come
/// before every door that can call the callee, in both helpers.
#[test]
fn both_compiled_call_helpers_judge_the_site_before_any_door() {
    for (sig, first_door) in [
        (
            "pub unsafe extern \"C\" fn jit_invoke_dispatch(",
            "try_lean_native_door(",
        ),
        (
            "pub unsafe extern \"C\" fn jit_invoke_virtual_mic(",
            "jit_invoke_bound_site_mic(",
        ),
    ] {
        let body = item(sig);
        let guard = body.find("jit_site_access_guard(vm_ptr, info_ptr, args_ptr, num_args)");
        let door = body.find(first_door);
        assert!(guard.is_some(), "{sig} does not run the site access guard");
        assert!(door.is_some(), "{sig}: `{first_door}` not found");
        assert!(guard < door, "{sig}: the guard runs after `{first_door}`");
    }
}

/// The guard settles an `IllegalAccessError` against the class entry's
/// record (later executions rethrow it) and loads an `invokestatic` owner
/// through the caller's loader before resolving, as the interpreter does.
#[test]
fn the_slow_guard_resolves_like_execute_invokestatic() {
    let body = item("unsafe fn jit_site_access_guard_slow(");
    let load = body.find("resolve_class_entry_loader_aware(");
    let resolve = body.find("resolve_method_ref_settling(");
    assert!(load.is_some() && resolve.is_some() && load < resolve);
    assert!(body.contains("settle_method_resolution_failure("));
    assert!(body.contains("recorded_member_owner_failure("));
    assert!(body.contains("CompileArgPins::pin_raw("));
}

fn site(class_name: &'static str, method_name: &'static str, caller: u32) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name,
        method_name,
        descriptor: "()I",
        num_jit_args: 0,
        return_type: 0,
        invoke_kind: 3,
        declaring_class_id: caller,
        owner_class_id: 0,
    }
}

#[test]
fn an_admitted_site_is_remembered_and_the_memo_is_bounded() {
    JIT_SITE_ACCESS_PASSED.with(|p| p.borrow_mut().clear());
    let a = site("p/A", "f", 7);
    let key = (1, 0x1000);
    jit_site_access_admit(key, &a);
    assert_eq!(JIT_SITE_ACCESS_LAST.with(std::cell::Cell::get).1, key);
    assert!(jit_site_access_admitted(key, &a));
    for i in 0..(1usize << 16) {
        jit_site_access_admit((2, i), &a);
    }
    assert!(JIT_SITE_ACCESS_PASSED.with(|p| p.borrow().len()) <= 1 << 16);
    JIT_SITE_ACCESS_PASSED.with(|p| p.borrow_mut().clear());
}

/// Failed before t1 wave 3 (`t1-V1-site-access-memo-keys-alias-after-reuse`):
/// the memo was keyed by the info and name-string ADDRESSES plus the caller
/// id, so a recompiled or redefined caller whose new site got the same
/// allocations back for another reference inherited "admitted". A hit now
/// names the same reference text, caller and kind, or it is no admission.
#[test]
fn a_recycled_address_naming_another_reference_is_not_admitted() {
    JIT_SITE_ACCESS_PASSED.with(|p| p.borrow_mut().clear());
    let key = (1, 0x1000);
    jit_site_access_admit(key, &site("p/A", "f", 7));
    // Same address, same lengths, another owner (the redefined caller's
    // new reference): judged again.
    assert!(!jit_site_access_admitted(key, &site("p/B", "g", 7)));
    // Same text, another caller class: another site.
    assert!(!jit_site_access_admitted(key, &site("p/A", "f", 8)));
    // Same text, another kind.
    let mut virt = site("p/A", "f", 7);
    virt.invoke_kind = 0;
    assert!(!jit_site_access_admitted(key, &virt));
    // The same reference at the same address (a recompile of the same
    // body) keeps its admission.
    assert!(jit_site_access_admitted(key, &site("p/A", "f", 7)));
    JIT_SITE_ACCESS_PASSED.with(|p| p.borrow_mut().clear());
}

/// The one-compare fast path holds only within one code-cache generation:
/// freeing an info takes an invalidation, which advances it.
#[test]
fn the_last_site_fast_path_is_scoped_to_one_cache_generation() {
    let body = item("unsafe fn jit_site_access_guard(");
    assert!(body.contains("let generation = cratonvm_jit::jit_cache_generation();"));
    assert!(body.contains("== (generation, key)"));
    assert!(body.contains("jit_site_access_admitted(key, info)"));
}

/// `t1-V1` secondary: under `--real-jdk` a failed owner load is the
/// resolution's error, recorded against the class entry (later executions
/// rethrow it) and raised at the call, not left for the dispatch to ask the
/// loader a second time.
#[test]
fn a_failed_owner_load_is_recorded_and_raised_under_jdk_only() {
    let body = item("unsafe fn jit_site_access_guard_slow(");
    let load = body.find("resolve_class_entry_loader_aware(");
    let record = body.find("record_member_owner_failure_as_of(");
    assert!(load.is_some() && record.is_some() && load < record);
    assert!(body.contains("convert_class_not_found("));
    assert!(body.contains("if !vm.config.is_jdk_only()"));
}

#[test]
fn multiple_distinct_sites_use_the_indexed_ring_in_guard_and_repeat() {
    let guard_body = item("unsafe fn jit_site_access_guard(");
    let repeat_body = item("pub unsafe fn jit_site_access_repeat(");
    assert!(guard_body.contains("JIT_SITE_ACCESS_RING.with("));
    assert!(guard_body.contains("jit_site_access_ring_slot(key)"));
    assert!(repeat_body.contains("JIT_SITE_ACCESS_RING.with("));
    assert!(repeat_body.contains("jit_site_access_ring_slot(key)"));
}
