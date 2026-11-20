//! zt t6/rv3 (`zgc-zt4x3-wave-t3-review-residuals-DECIDED-20261019.md` item 1):
//! `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH`'s narrow flush, one epoch at a
//! time, and its switch reached by a thread override.
use super::*;

type Template = Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)>;

fn template() -> Template {
    Some((
        std::sync::Arc::new(crate::classloading::resolution::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(77),
                class_name: std::sync::Arc::from("com/example/Callee"),
                method_name: std::sync::Arc::from("m"),
                method_descriptor: std::sync::Arc::from("()V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&[0xb1]),
                exception_table: std::sync::Arc::from(Vec::new()),
                max_stack: 0,
                max_locals: 0,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        )),
        crate::classloading::resolution::RedefineGate::never_stale(),
    ))
}

#[test]
fn compiled_template_warmup_survives_template_invalidation() {
    use std::sync::atomic::Ordering;
    let site = jit_site_key(0x7171, 0x1234);
    let count = compiled_template_warmup(site, 7);
    count.store(47, Ordering::Relaxed);
    clear_publication_sensitive_site_memos();
    narrow_dispatch_memo_flush((0x7171, 1, 1, 1, false), 1);
    let rebuilt = compiled_template_warmup(site, 7);
    assert!(std::sync::Arc::ptr_eq(&count, &rebuilt));
    assert_eq!(rebuilt.load(Ordering::Relaxed), 47);
    assert_eq!(compiled_template_warmup(site, 8).load(Ordering::Relaxed), 0);
    assert_eq!(compiled_template_warmup(jit_site_key(0x7172, 0x1234), 7).load(Ordering::Relaxed), 0);
    clear_site_keyed_dispatch_memos();
    assert_eq!(compiled_template_warmup(site, 7).load(Ordering::Relaxed), 0);
    clear_site_keyed_dispatch_memos();
}

/// `answer`: a positive entry in each bytecode-callee memo. `refused`: a
/// cached refusal in each, a wrapped-entry refusal and a
/// `selection_error` virtual target.
fn fill(answer: JitSiteKey, refused: JitSiteKey) {
    STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.insert(answer, template());
        c.insert(refused, None);
    });
    VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.insert((answer, 7), template());
        c.insert((refused, 7), None);
    });
    SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.insert(answer, template());
        c.insert(refused, None);
    });
    WRAPPED_STATIC_CALLEE.with(|c| {
        c.borrow_mut().insert(refused, ());
    });
    VIRTUAL_TARGET_CACHE.with(|c| {
        c.borrow_mut().insert(
            (refused, 7),
            CachedDispatchTarget {
                class_name: std::rc::Rc::from("com/example/Owner"),
                cacheable_receiver: true,
                globally_named: true,
                selection_error: true,
                selected_cid: 0,
            },
        );
    });
}

/// `(static, virtual, special)` memo entry of `site`: `Some(true)` a
/// positive entry, `Some(false)` a refusal, `None` no entry.
fn entries(site: JitSiteKey) -> [Option<bool>; 3] {
    [
        STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&site).map(|v| v.is_some())),
        VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&(site, 7)).map(|v| v.is_some())),
        SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&site).map(|v| v.is_some())),
    ]
}

fn wrapped(site: JitSiteKey) -> bool {
    WRAPPED_STATIC_CALLEE.with(|c| c.borrow().contains_key(&site))
}

fn selection_error_kept(site: JitSiteKey) -> bool {
    VIRTUAL_TARGET_CACHE.with(|c| c.borrow().contains_key(&(site, 7)))
}

#[test]
fn zt6rv3_a_definition_only_move_drops_refusals_and_keeps_answers() {
    // A `vm_identity` no test VM is issued (a counter from 1).
    const VM: usize = usize::MAX - 61;
    let answer = jit_site_key(VM, 0x1000);
    let refused = jit_site_key(VM, 0x2000);
    let state = (VM, 4, 5, 6, false);
    narrow_dispatch_memo_flush(state, 10);
    fill(answer, refused);

    // Nothing moved: nothing is dropped.
    narrow_dispatch_memo_flush(state, 10);
    assert_eq!(entries(refused), [Some(false); 3]);
    assert!(wrapped(refused));

    // Only the definition epoch moved (a brand-new class): the refusals
    // go, the answers and the `selection_error` target stay.
    narrow_dispatch_memo_flush(state, 11);
    assert_eq!(entries(answer), [Some(true); 3], "answers survive");
    assert_eq!(entries(refused), [None; 3], "refusals dropped");
    assert!(!wrapped(refused), "the wrapped-entry refusal is dropped");
    assert!(selection_error_kept(refused), "selection_error rows are kept");

    // Positive control: a second definer of an existing name moves the
    // name generation, which flushes every memo.
    fill(answer, refused);
    narrow_dispatch_memo_flush((VM, 5, 5, 6, false), 12);
    assert_eq!(entries(answer), [None; 3], "full flush");
    assert_eq!(entries(refused), [None; 3]);
    assert!(!wrapped(refused));
    assert!(!selection_error_kept(refused));

    // And each other keyed epoch flushes in full too.
    for moved in [(VM, 5, 6, 6, false), (VM, 5, 6, 7, false), (VM, 5, 6, 7, true)] {
        fill(answer, refused);
        narrow_dispatch_memo_flush(moved, 12);
        assert_eq!(entries(answer), [None; 3], "{moved:?}");
    }
}

