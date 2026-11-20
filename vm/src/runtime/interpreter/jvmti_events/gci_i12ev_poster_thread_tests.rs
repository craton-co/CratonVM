//! gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
//! item 1c), source pins (the posters are `experimental-debug` code; these
//! read the files, so they run in every build): the thread start / end,
//! class prepare, lambda-proxy class and allocation posters deliver to C
//! agents under `post_on_own_thread` / `post_on_own_thread_ptr`, and
//! the thread-bearing class load runs under it while a C env listens for
//! `ClassLoad`.

fn normalized(raw: &str) -> String {
    raw.replace("\r\n", "\n")
}

/// The text of the item starting at `start` up to its closing `end`.
fn item<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find(end).unwrap_or_else(|| panic!("the end of {start}"))]
}

/// Every occurrence of `call` in `src` sits inside a `wrap` opened at
/// most `reach` bytes before it; answers how many there are.
fn each_wrapped(src: &str, call: &str, wrap: &str, reach: usize) -> usize {
    let mut seen = 0;
    let mut from = 0;
    while let Some(at) = src[from..].find(call) {
        let at = from + at;
        // The files hold non-ASCII text: start the window on a char.
        let mut lo = at.saturating_sub(reach);
        while !src.is_char_boundary(lo) {
            lo += 1;
        }
        let window = &src[lo..at];
        assert!(window.contains(wrap), "`{call}` outside `{wrap}` at byte {at}");
        seen += 1;
        from = at + call.len();
    }
    seen
}

#[test]
fn gci_i12ev_thread_start_and_end_post_on_the_workers_own_handle() {
    let src = normalized(concat!(include_str!("../../../vm/vm_exec/mod.rs"), "\n", include_str!("../../../vm/vm_exec/method.rs"), "\n", include_str!("../../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../../vm/vm_exec/jni_table_native_call.rs")));
    let posts = each_wrapped(
        &src,
        "crate::jvmti::native_env::post_thread_event(&shared_arc,",
        "crate::runtime::interpreter::post_on_own_thread(&mut jvm_thread, || {",
        120,
    );
    assert_eq!(posts, 2, "ThreadStart and ThreadEnd");
    // A virtual thread's start / mount / unmount / end, on its own handle
    // (`resume_virtual_continuation`).
    let wrap = "crate::runtime::interpreter::post_on_own_thread(&mut thread, || {";
    let posts = each_wrapped(&src, "crate::jvmti::native_env::post_virtual_thread(", wrap, 120);
    assert_eq!(posts, 2, "a resumed mount, a park's unmount");
    let posts = each_wrapped(&src, "post_virtual_thread(&shared, tid.0, VirtualThreadEvent::", wrap, 220);
    assert_eq!(posts, 4, "start + mount, unmount + end");
    // `VirtualThread.notifyJvmti*`'s native (`jdk_continuation.rs`).
    let cont = normalized(include_str!("../../../vm/jdk_continuation.rs"));
    let body = item(&cont, "pub(crate) fn post_transition(", "\n}\n");
    let wrap = body
        .find("crate::runtime::interpreter::post_on_own_thread(ctx.thread, || match kind {")
        .expect("the wrap");
    let first = body.find("post_virtual_thread(&shared_arc,").expect("a post");
    assert!(wrap < first);
}

#[test]
fn gci_i12ev_class_prepare_posts_on_the_preparing_threads_handle() {
    let src = normalized(include_str!("../../../vm/vm_util.rs"));
    let body = item(&src, "\nfn link_claimed_class(", "\n}\n");
    let wrap = body
        .find("crate::runtime::interpreter::post_on_own_thread(thread, || {")
        .expect("the wrap");
    let post = body
        .find("crate::debug::class_prepared_on_thread(shared, tid, class_id)")
        .expect("the post");
    assert!(wrap < post && post - wrap < 320, "the post is inside the wrap");
    // gci i12/ev second pass: deferred `ClassLoad`s first, inside it.
    let flush = body
        .find("crate::jvmti::native_env::flush_deferred_class_loads(shared, tid);")
        .expect("the flush");
    assert!(wrap < flush && flush < post);
    assert_eq!(body.matches("class_prepared_on_thread(").count(), 1, "one post");
    // The lambda proxy's class events, in `bootstrap_lambda`.
    let src = normalized(concat!(include_str!("../../invokedynamic/mod.rs"), "\n", include_str!("../../invokedynamic/dbg_indy_all.rs"), "\n", include_str!("../../invokedynamic/execute_invokedynamic_at_uncensused.rs"), "\n", include_str!("../../invokedynamic/object_methods_getter_refusal.rs"), "\n", include_str!("../../invokedynamic/descriptor_param_count_and_last_is.rs")));
    let posts = each_wrapped(
        &src,
        "crate::debug::lambda_proxy_minted(",
        "crate::runtime::interpreter::post_on_own_thread(thread, || {",
        120,
    );
    assert_eq!(posts, 1, "the proxy's ClassPrepare / ClassLoad");
}

#[test]
fn gci_i12ev_a_thread_bearing_class_load_installs_its_handle_for_class_load_agents() {
    let src = normalized(concat!(include_str!("../../../vm/vm_init/mod.rs"), "\n", include_str!("../../../vm/vm_init/missing_native_entry.rs"), "\n", include_str!("../../../vm/vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../../../vm/vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../../../vm/vm_init/wait_site.rs"), "\n", include_str!("../../../vm/vm_init/class_store_cut_with.rs")));
    let body = item(&src, "pub fn load_class_transformed(", "\n    }\n");
    let gate = body.find("debugger_gates.native_class_load_armed()").expect("the gate");
    let wrap = body
        .find("crate::runtime::interpreter::post_on_own_thread(thread, || {")
        .expect("the wrap");
    let load = body.find("self.load_class_concurrent(name)").expect("the load");
    assert!(gate < wrap && wrap < load, "gate, wrap, then the load inside it");
    assert!(
        body.ends_with("self.load_class_concurrent(name)"),
        "no listener: the plain load"
    );
    assert!(body.contains("#[cfg(feature = \"experimental-debug\")]"));
    // The define natives' single backend: the define (whose guard drop
    // posts `ClassLoad`) touches `shared` only, and runs under the wrap.
    let src = normalized(concat!(include_str!("../../../vm/vm_exec/mod.rs"), "\n", include_str!("../../../vm/vm_exec/method.rs"), "\n", include_str!("../../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../../vm/vm_exec/jni_table_native_call.rs")));
    let body = item(&src, "    fn define_class_full(", "\n    }\n");
    // gci i21/fx: the 2026-10-15 merge brought dev's lane superAcc, whose
    // closure returns the raw `VmError` so the unnamed-module hash (which
    // needs `self`) is appended after it -- the define still reaches no
    // `self`, and the refusal is named outside the wrap.
    let open = body
        .find("let define = || -> Result<(ClassId, ClassId), VmError> \u{7b}")
        .expect("the define closure");
    let close = body.find("Ok((cid, next_id_before))").expect("its end");
    assert!(open < close);
    assert!(!body[open..close].contains("self."), "the define reaches no `self`");
    let gate = body.find("debugger_gates.native_class_load_armed()").expect("the gate");
    let wrap = body
        .find("crate::runtime::interpreter::post_on_own_thread(self.thread, define)")
        .expect("the wrap");
    assert!(close < gate && gate < wrap);
    let taken = body
        .find("let (cid, next_id_before) = match defined \u{7b}")
        .expect("the define's answer taken after the wrap");
    let named = body
        .find("self.define_refusal_with_unnamed_module_hash(e, cl_id)")
        .expect("a refusal named outside the closure");
    assert!(wrap < taken && taken < named);
}

#[test]
fn gci_i12ev_allocation_events_install_the_allocating_threads_pointer() {
    let src = normalized(include_str!("../../../jvmti/native_env/events_more.rs"));
    let body = item(&src, "\nfn post_alloc(", "\n}\n");
    let ptr = body.find("let thread_ptr: *mut JvmThread = thread;").expect("one pointer");
    let deliver = body.find("let deliver = || {").expect("the deliveries");
    let post = body
        .find("crate::runtime::interpreter::post_on_own_thread_ptr(thread_ptr, deliver)")
        .expect("installed around them");
    assert!(ptr < deliver && deliver < post);
    assert_eq!(body.matches("&mut *thread_ptr").count(), 1, "alloc_facts' borrow only");
    // `ResourceExhausted`, posted where the VM builds an OOME.
    let src = normalized(include_str!("../../exceptions.rs"));
    let posts = each_wrapped(
        &src,
        "crate::jvmti::native_env::post_resource_exhausted(",
        "crate::runtime::interpreter::post_on_own_thread(thread, || {",
        120,
    );
    assert_eq!(posts, 1);
}

/// gci i12/ev second pass: the deferred-`ClassLoad` flush points
/// (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`) and the `NativeMethodBind` /
/// `ClassFileLoadHook` posters.
#[test]
fn gci_i12ev_flush_points_and_the_bind_and_hook_posters_use_the_own_handle() {
    let exec = normalized(concat!(
        include_str!("../../../vm/vm_exec/mod.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/method.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/report_pad_access.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/load_class.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/new_object.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/invoke_special_shared.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"),
        "\n",
        include_str!("../../../vm/vm_exec/jni_table_native_call.rs")
    ));
    // The native funnel's binding: flush once the own handle is installed.
    let install = item(
        &exec,
        "unsafe fn install(shared: &SharedVm, thread: *mut JvmThread) -> Self {",
        "\n    }\n",
    );
    let bound = install
        .find("replace_jni_thread(thread)")
        .expect("the binding");
    let flush = install
        .find("crate::jvmti::native_env::flush_deferred_class_loads(shared, tid);")
        .expect("the flush");
    assert!(bound < flush);
    // The platform and the virtual thread's last own-handle points.
    let flushes = each_wrapped(
        &exec,
        "crate::jvmti::native_env::flush_deferred_class_loads(&shared",
        "crate::runtime::interpreter::post_on_own_thread(&mut ",
        120,
    );
    assert_eq!(flushes, 2, "thread end, virtual thread end");
    // The three compatibility define doors define on the own handle, and
    // their defines reach no `self`.
    for door in [
        "    fn define_class_from_bytes(",
        "    fn define_hidden_class_from_bytes(",
        "    fn define_class_with_loader(",
    ] {
        let body = item(&exec, door, "\n    }\n");
        let opener = "define_on_own_handle(shared, self.thread, || {";
        let wrap = body.find(opener).unwrap_or_else(|| panic!("{door}"));
        assert!(
            !body[wrap + opener.len()..].contains("self."),
            "{door}: the define reaches no `self`"
        );
    }
    let helper = item(&exec, "fn define_on_own_handle<R>(", "\n}\n");
    assert!(helper.contains("native_class_load_armed()"));
    assert!(helper.contains("crate::runtime::interpreter::post_on_own_thread(thread, define)"));
    // `post_rust_native_method_bind` posts under the funnel's handle.
    let more = normalized(include_str!("../../../jvmti/native_env/events_more.rs"));
    let bind = item(&more, "pub(crate) fn post_rust_native_method_bind(", "\n}\n");
    // gci i21/fx: the signature compared with its whitespace removed --
    // rustfmt splits it one parameter per line once it outgrows a line,
    // which is the layout the file has now; the parameters are the pin.
    let sig: String = bind[..bind.find(") -> bool").expect("the signature")]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        sig.starts_with(
            "pub(crate)fnpost_rust_native_method_bind(shared:&SharedVm,thread:&mutJvmThread,"
        ),
        "{sig}"
    );
    let wrap = bind
        .find("crate::runtime::interpreter::post_on_own_thread(thread, || {")
        .expect("the wrap");
    let post = bind.find("post_native_method_bind(shared, tid,").expect("the post");
    assert!(wrap < post);
    // The C `ClassFileLoadHook` offer runs inside the context's binding.
    let inst = normalized(include_str!("../../instrument.rs"));
    let binding = inst.find("ctx.run_with_jni_binding(&mut || {").expect("the binding");
    let offer = inst
        .find("answer = post_native_class_file_load_hook(")
        .expect("the offer");
    assert!(binding < offer && offer - binding < 120);
}
