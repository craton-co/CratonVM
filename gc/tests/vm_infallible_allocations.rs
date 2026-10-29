// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Keep the explicit fatal allocation boundary from leaking into Java paths.
//! Parse Rust instead of cutting files at their first test module: production
//! functions occur after inline tests, and calls can span multiple lines.
use std::collections::BTreeMap;
use std::path::Path;
use syn::visit::{self, Visit};

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Meta>().is_ok_and(|m| required_test(&m)))
    })
}

fn required_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(p) => p.is_ident("test"),
        syn::Meta::List(l) if l.path.is_ident("all") => {
            use syn::parse::Parser;
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
                .parse2(l.tokens.clone())
                .is_ok_and(|ms| ms.iter().any(required_test))
        }
        _ => false,
    }
}

#[derive(Default)]
struct Calls {
    function: String,
    calls: BTreeMap<String, usize>,
    contexts: BTreeMap<String, usize>,
    infallible_mirrors: BTreeMap<String, usize>,
}
impl<'ast> Visit<'ast> for Calls {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if !test_only(&m.attrs) {
            visit::visit_item_mod(self, m);
        }
    }
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if test_only(&f.attrs) {
            return;
        }
        let old = std::mem::replace(&mut self.function, f.sig.ident.to_string());
        visit::visit_item_fn(self, f);
        self.function = old;
    }
    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if test_only(&f.attrs) {
            return;
        }
        let old = std::mem::replace(&mut self.function, f.sig.ident.to_string());
        visit::visit_impl_item_fn(self, f);
        self.function = old;
    }
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*call.func {
            if path.path.segments.last().is_some_and(|s| {
                s.ident == "get_or_create_class_mirror"
                    || s.ident == "get_or_create_primitive_mirror"
            }) {
                *self
                    .infallible_mirrors
                    .entry(self.function.clone())
                    .or_default() += 1;
            }
        }
        visit::visit_expr_call(self, call);
    }
    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        if expr
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident == "NativeContextImpl")
        {
            *self.contexts.entry(self.function.clone()).or_default() += 1;
        }
        visit::visit_expr_struct(self, expr);
    }
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "alloc_object"
            || call.method == "alloc_array"
            || call.method == "ensure_system_streams"
            || call.method == "get_class_lock_object"
        {
            *self.calls.entry(self.function.clone()).or_default() += 1;
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn scan(dir: &Path, root: &Path, found: &mut BTreeMap<(String, String), usize>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            // A `tests/` module directory is test code end to end, the same
            // rule as a `tests.rs` file: `vm/src/vm/tests.rs` (declared
            // `#[cfg(all(test, feature = "synthetic-jdk"))] mod tests;` in
            // `vm/src/vm.rs`) became `vm/src/vm/tests/*.rs` in `6270b78141`,
            // whose files carry no `cfg(test)` of their own.
            if path.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            scan(&path, root, found);
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy();
        if path.extension().is_none_or(|e| e != "rs")
            || name == "tests.rs"
            || name.ends_with("_tests.rs")
        {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let ast = syn::parse_file(&src).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut calls = Calls::default();
        calls.visit_file(&ast);
        let file = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        for (function, count) in calls.calls {
            found.insert((file.clone(), function), count);
        }
    }
}

#[test]
fn java_paths_do_not_add_infallible_allocation_calls() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("vm/src");
    let mut actual = BTreeMap::new();
    scan(&root, &root, &mut actual);
    // Each exception is a reviewed legacy bare-ObjectRef API or a collector
    // fallback behind an explicit policy gate. No growth is permitted.
    let expected: BTreeMap<(String, String), usize> = [
        ("vm/vm_exec.rs", "heap_alloc_object", 1),
        ("vm/vm_exec.rs", "new_array", 1),
        ("vm/vm_exec.rs", "new_ref_array", 1),
        ("vm/vm_init.rs", "new_array", 1),
        ("vm/vm_init.rs", "new_ref_array", 1),
        ("vm/vm_object.rs", "alloc_class_mirror_object", 1),
    ]
    .into_iter()
    .map(|(p, f, n)| ((p.into(), f.into()), n))
    .collect();
    assert_eq!(
        actual, expected,
        "Use fallible allocation and a rooted OOME ladder; review and shrink fatal exceptions"
    );
}

#[test]
fn scanner_covers_production_after_tests_and_multiline_calls() {
    let ast = syn::parse_file(
        r#"
        fn before() { heap.alloc_object(id, 0); }
        #[cfg(test)] mod tests { fn fixture() { heap.alloc_array(id, kind, 1); } }
        impl Runtime { fn after() { heap
 .alloc_array(id, kind, 2); } }
    "#,
    )
    .unwrap();
    let mut calls = Calls::default();
    calls.visit_file(&ast);
    assert_eq!(
        calls.calls,
        [("before".into(), 1), ("after".into(), 1)]
            .into_iter()
            .collect()
    );
}

#[test]
fn shipped_collectors_report_refusal_through_the_trait() {
    use cratonvm_gc::g1::{G1Collector, G1CollectorConfig};
    use cratonvm_gc::{GarbageCollector, GenerationalHeap};
    use cratonvm_types::{ArrayElementType, ClassId};
    let mut heaps: Vec<Box<dyn GarbageCollector>> = vec![
        Box::new(GenerationalHeap::with_capacity(16 << 20)),
        Box::new(G1Collector::new(G1CollectorConfig {
            heap_size: 16 << 20,
            ..G1CollectorConfig::default()
        })),
    ];
    #[cfg(feature = "zgc")]
    heaps.push(Box::new(cratonvm_gc::zgc::ZgcRealHeap::with_capacity(
        16 << 20,
    )));
    for heap in heaps {
        assert!(heap.try_alloc_object(ClassId::new(0), 0).is_some());
        assert!(heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 8)
            .is_some());
        assert!(heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 << 20)
            .is_none());
        assert!(heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Reference, usize::MAX)
            .is_none());
        assert!(heap.try_alloc_object(ClassId::new(0), usize::MAX).is_none());
        while heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 4096)
            .is_some()
        {}
        while heap.try_alloc_object(ClassId::new(0), 0).is_some() {}
        assert!(heap.try_alloc_object(ClassId::new(0), 0).is_none());
        assert!(heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 0)
            .is_none());
    }
}

#[test]
fn direct_jit_native_contexts_require_an_allocation_audit() {
    let helper_slices = [
        include_str!("../../vm/src/jit/helpers/thread.rs"),
        include_str!("../../vm/src/jit/helpers/alloc.rs"),
        include_str!("../../vm/src/jit/helpers/access.rs"),
        include_str!("../../vm/src/jit/helpers/typecheck.rs"),
        include_str!("../../vm/src/jit/helpers/dispatch.rs"),
        include_str!("../../vm/src/jit/helpers/fast_native.rs"),
        include_str!("../../vm/src/jit/helpers/deopt.rs"),
        include_str!("../../vm/src/jit/helpers/table.rs"),
    ];
    let mut found = Calls::default();
    for src in helper_slices {
        found.visit_file(&syn::parse_file(src).unwrap());
    }
    // Each body and its callees are reviewed in the retirement report. Read
    // paths cannot acquire an allocating callee without updating that audit.
    // FFM uses a local once-only OOME scope; boxing uses the collecting ladder.
    let expected: BTreeMap<String, usize> = [
        ("mh_compiled_link_redirect", 1),
        ("mh_constant_bind_fill", 1),
        ("try_jit_arraylist_leaf", 7),
        ("ffm_downcall_site_fill", 1),
        ("ffm_downcall_lean_call", 1),
        ("ffm_downcall_direct_call", 1),
        ("try_varhandle_byte_buffer_view_access", 1),
        ("try_varhandle_instance_field_read", 1),
        ("try_varhandle_segment_access", 1),
        ("jit_box_cold_alloc", 3),
        ("buffer_stored_session", 1),
        ("jit_unsafe_get_volatile_body", 1),
        ("jit_unsafe_put_volatile_body", 1),
        ("jit_refers_to0_body", 1),
        ("jit_concurrent_hashmap_get_direct_body", 1),
        ("jit_hashmap_get_direct_body", 2),
        ("jit_hashmap_put_direct_body", 1),
        ("call_integer_native_raw_inner", 1),
        ("real_seg_fill", 1),
        // The MethodHandle real-field lane (2c274d3af7): a field read through
        // `jit_real_field_lane_warm`; its errors go through
        // `handle_jit_dispatch_error`.
        ("try_jit_mh_real_field_lane", 1),
        // Audited JIT round 22 wave 3, lane review (each body's doc states it):
        // * `compiled_user_loader_resource_body` (interpreter i6 wave 132): the
        //   context only asks `user_loader_resource_body_for_class`, a class-store
        //   read -- no allocation, no load;
        // * `reflect_accessor_link_redirect` (JIT r21): the member's call through
        //   `mh_bind_direct_call`; a throwable or deopt comes back as the
        //   `i64::MIN` sentinel with its signal raised, unchanged;
        // * `reflect_ctor_link_redirect` (JIT r21): the allocation's
        //   `OutOfMemoryError` is left pending and answered with the sentinel
        //   (`set_jit_deopt_pending` on the no-thread arm), the arguments and the
        //   instance pinned across both GC points;
        // * `jit_class_get_name_direct_body` (JIT r22 wave 2): a leaf read of the
        //   strongly held memo answer (`class_get_name_fast`); every miss
        //   declines to the dispatch, which runs the native.
        ("compiled_user_loader_resource_body", 1),
        ("reflect_accessor_link_redirect", 1),
        ("reflect_ctor_link_redirect", 1),
        ("jit_class_get_name_direct_body", 1),
        // JIT round 22 wave 3 (lane inline; audited wave 6, lane review):
        // `mh_bind_customization_serves` -- the context only runs
        // `lang_invoke::lane_check_customized`, slot reads and one `int`
        // count write on the handle; no allocation, no Java call (a call that
        // is due declines to the door, whose JDK linker customizes).
        ("mh_bind_customization_serves", 1),
        // JIT round 23 wave 2d, lane L8 (audited wave 3): `ffm_heap_segment_access`
        // -- the context runs only `lang_invoke::segment_layout_heap_access`:
        // the layout's memoised plan (`seg_layout_plan`, the slot reads
        // `real_seg_fill`'s `segment_layout_facts` already makes), the heap
        // segment's facts (`seg_heap_facts`: field and class-name reads, no
        // mirror minted -- `current_thread_object_if_any`), the session's
        // `state` / `owner` read and one primitive array element read or
        // write. No Java allocation, no Java call, nothing raised: every
        // failing check declines (`None`) to the site's unchanged dispatch.
        // (Wave 3's `segment_layout_heap_facts` runs inside `real_seg_fill`'s
        // existing context, on the same reads.)
        ("ffm_heap_segment_access", 1),
        // JIT round 23 wave 4, lane L5 (audited by the wave-4 review):
        // `string_regex_direct_serve` (`CRATONVM_JIT_STRING_REGEX_DIRECT`) -- the
        // context runs only `lang_string::string_regex_thin`, the registered
        // `String.matches` / `replaceAll` / `replaceFirst` natives' exact path:
        // string reads and, for a replacement that changed the text, ONE result
        // `String` (`create_string_uninterned`, which never collects). It runs
        // under `native_oom::catch_alloc_oom_on`, so a refused allocation is a
        // decline, not the infallible arm's abort; every decline and every
        // exception dispatches the native through the funnel with the
        // unchanged argument words. No Java call.
        ("string_regex_direct_serve", 1),
    ]
    .into_iter()
    .map(|(name, n)| (name.into(), n))
    .collect();
    assert_eq!(
        found.contexts, expected,
        "audit every direct context's Java allocation/error path"
    );
}

#[test]
fn class_preseeds_and_fixups_use_only_fallible_mirror_factories() {
    let src = include_str!("../../vm/src/vm/vm_util.rs");
    let mut calls = Calls::default();
    calls.visit_file(&syn::parse_file(src).unwrap());
    assert!(
        calls.infallible_mirrors.is_empty(),
        "{:?}",
        calls.infallible_mirrors
    );
}
