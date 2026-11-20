//! gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
//! item 1, part 2): a poster installs its own handle around the delivery
//! and puts the previous binding back, also on unwind.
use super::*;
use crate::native::jni::{installed_jni_thread_addr, replace_jni_thread, restore_jni_thread};
use crate::threading::jvm_thread::ThreadId;

#[test]
fn gci_i11ja_a_post_runs_with_the_posters_thread_installed() {
    std::thread::spawn(|| {
        let mut poster = JvmThread::new(ThreadId(7), "poster");
        let own = &mut poster as *mut JvmThread as usize;
        assert_eq!(installed_jni_thread_addr(), None, "a fresh OS thread");
        let seen = post_on_own_thread(&mut poster, installed_jni_thread_addr);
        assert_eq!(seen, Some(own), "the poster's own handle, during the post");
        assert_eq!(installed_jni_thread_addr(), None, "nothing left behind");
        // Under an outer binding (a native funnel's): put back exactly.
        let mut outer = JvmThread::new(ThreadId(8), "outer");
        let outer_addr = &mut outer as *mut JvmThread as usize;
        // SAFETY: `outer` lives until the restore below.
        let prev = unsafe { replace_jni_thread(&mut outer as *mut JvmThread) };
        let seen = post_on_own_thread(&mut poster, installed_jni_thread_addr);
        assert_eq!(seen, Some(own));
        assert_eq!(installed_jni_thread_addr(), Some(outer_addr), "the outer binding");
        // An unwinding post restores too.
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            post_on_own_thread(&mut poster, || -> u8 { panic!("simulated callback unwind") })
        }));
        assert!(unwound.is_err());
        assert_eq!(installed_jni_thread_addr(), Some(outer_addr), "restored on unwind");
        // SAFETY: what this thread had installed before `outer`.
        unsafe { restore_jni_thread(prev) };
        assert_eq!(installed_jni_thread_addr(), None);
    })
    .join()
    .expect("poster thread");
}

/// Source pin: every poster in this file that holds the thread's `&mut`
/// delivers through the helper; the two that also hold a
/// `NativeFrameRow` share the row's pointer.
#[test]
fn gci_i11ja_mut_posters_install_their_own_handle() {
    let src = include_str!("../jvmti_events.rs").replace("\r\n", "\n");
    let item = |name: &str| -> String {
        let start = src.find(name).unwrap_or_else(|| panic!("{name}"));
        let rest = &src[start..];
        rest[..rest.find("\n}\n").expect("its end")].to_string()
    };
    for f in [
        "pub(super) fn fire_jvmti_frame_pop_if_requested(",
        // The body both entry hooks share since origin/dev's wave 95
        // (`fire_method_entry_after_push` / `_in_loop` delegate to it).
        "\nfn fire_method_entry_after_push_with(",
        "\nfn fire_method_entry_at_callback_point(",
        "pub(crate) fn fire_deferred_method_entry(",
    ] {
        let b = item(f);
        assert!(b.contains("post_on_own_thread(thread, || {"), "{f}");
        let post = b.find("post_on_own_thread(").expect("post");
        let fire = b.find("crate::runtime::jvmti::fire_").expect("fire");
        assert!(post < fire, "{f}: the fire is inside the post");
    }
    let entry = item("fn report_named_native_method_entry(");
    assert!(entry.contains("enter_hashed(shared, own, declaring, method_id)"));
    assert!(entry.contains("post_on_own_thread_ptr(own, || {"));
    let exit = item("pub(crate) fn report_native_method_exit(");
    assert!(exit.contains("post_on_own_thread_ptr(own, || {"));
    assert!(exit.contains("post_on_own_thread(thread, || {"));
    assert!(!exit.contains("thread as *mut JvmThread,\n"), "the row takes `own`");
}

/// gci i11/ja item 3 (source pin; `jni.rs` lives outside this module): JNI
/// `MonitorEnter`'s unbound arm makes no `&mut` of the registry's
/// `JvmThread` for a caller already in its blocked region, which acquires
/// as an excluded thread; only the counted case still takes it.
#[test]
fn gci_i11ja_unbound_monitor_enter_spares_a_blocked_caller_the_mut() {
    let src = concat!(include_str!("../../../native/jni/mod.rs"), "\n", include_str!("../../../native/jni/j_boolean.rs"), "\n", include_str!("../../../native/jni/raw_locals_counted.rs"), "\n", include_str!("../../../native/jni/truncate_local_frames.rs"), "\n", include_str!("../../../native/jni/jni_empty_array_elements.rs"), "\n", include_str!("../../../native/jni/lambda_proxy_jni_construct.rs")).replace("\r\n", "\n");
    let start = src.find("extern \"C\" fn jni_monitor_enter(").expect("MonitorEnter");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n}\n").expect("its end")];
    let arm = body.find("if let Some(thread) = unbound_caller_jvm_thread(shared)").expect("arm");
    let arm = &body[arm..];
    let view = arm.find("&*(thread as *const JvmThread)").expect("a shared view");
    let excluded = arm.find("jni_monitor_enter_excluded(shared, obj, oref, me)?;").expect("excluded");
    let through = arm
        .find("jni_monitor_enter_through_downcall(shared, me, obj, oref)")
        .expect("the downcall's own binding first");
    let counted = arm
        .find("jni_monitor_enter_as(shared, unsafe { &mut *thread }, obj, oref)?;")
        .expect("counted");
    assert!(view < excluded && excluded < through && through < counted);
    assert_eq!(
        arm[..counted].matches("&mut *thread").count(),
        0,
        "no `&mut` before the fallback"
    );
    // The helper borrows through the binding the downcall's context installs.
    let start = src
        .find("\npub(in crate::native::jni) fn jni_monitor_enter_through_downcall(")
        .expect("helper");
    let rest = &src[start + 1..];
    let helper = &rest[..rest.find("\n}\n").expect("its end")];
    let ctx = helper
        .find("panama_libffi::with_active_context(")
        .expect("the downcall context");
    let binding = helper
        .find("ctx.run_with_jni_binding(")
        .expect("its binding");
    let bound = helper.find("with_jni_context(").expect("the bound thread");
    assert!(ctx < binding && binding < bound);
    assert!(
        helper.contains("ctx.vm_identity() != vm"),
        "this VM's downcall only"
    );
    assert!(!helper.contains("unsafe"), "no raw re-derive in the helper");
}

/// gci i11/ja item 1b (source pin): every C-agent delivery
/// (`crate::runtime::jvmti::fire_*`) in the interpreter's posting files
/// runs under `post_on_own_thread` / `post_on_own_thread_ptr`.
#[test]
fn gci_i11ja_every_interpreter_poster_installs_its_own_handle() {
    let files = [
        ("jvmti_events.rs", include_str!("../jvmti_events.rs")),
        ("../interpreter.rs", concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs"))),
        ("opcodes.rs", include_str!("../opcodes.rs")),
        ("lambda.rs", include_str!("../lambda.rs")),
        ("exception_dispatch.rs", include_str!("../exception_dispatch.rs")),
    ];
    let needle = "crate::runtime::jvmti::fire_";
    let mut seen = 0;
    for (name, raw) in files {
        let src = raw.replace("\r\n", "\n");
        // This file's test modules quote the needle: production only.
        let src = if name == "jvmti_events.rs" {
            let cut = src.find("\n#[cfg(test)]\n").expect("tests");
            src[..cut].to_string()
        } else {
            src
        };
        let mut from = 0;
        while let Some(at) = src[from..].find(needle) {
            let at = from + at;
            // gci i12/ev: the files hold non-ASCII text; start the
            // window on a char boundary (a mid-char slice panics).
            let mut lo = at.saturating_sub(400);
            while !src.is_char_boundary(lo) {
                lo += 1;
            }
            let window = &src[lo..at];
            assert!(
                window.contains("post_on_own_thread"),
                "{name}: a delivery outside the poster's own handle at byte {at}"
            );
            seen += 1;
            from = at + needle.len();
        }
    }
    assert!(seen >= 20, "the posters were found ({seen})");
}

/// gci i11/ja item 3: the VM's `run_with_jni_binding` installs the
/// context's own thread for the closure and restores the previous
/// binding (nothing) afterwards.
#[test]
fn gci_i11ja_run_with_jni_binding_installs_the_contexts_thread() {
    use cratonvm_native_api::NativeThreadAccess;
    std::thread::spawn(|| {
        let shared = std::sync::Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        *shared.self_arc.write() = Some(std::sync::Arc::downgrade(&shared));
        let mut thread = JvmThread::new(ThreadId(9), "downcall");
        let own = &mut thread as *mut JvmThread as usize;
        let mut seen = None;
        {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.run_with_jni_binding(&mut || seen = installed_jni_thread_addr());
        }
        assert_eq!(seen, Some(own), "the context's thread, during the closure");
        assert_eq!(installed_jni_thread_addr(), None, "the previous binding, after");
    })
    .join()
    .expect("binding thread");
}
