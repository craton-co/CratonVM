use super::{
    code_matches_shape, DELEGATE_INIT_EMPTY, DELEGATE_INIT_STRING, NPE_FILL_IN_STACK_TRACE,
    THROWABLE_FILL_IN_STACK_TRACE, THROWABLE_INIT_EMPTY, THROWABLE_INIT_STRING,
};

/// `javap -c` of JDK 25.0.3 `java.lang.Throwable(String)`, constant-pool
/// indices as that build numbers them.
const JDK25_THROWABLE_INIT_STRING: [u8; 48] = [
    0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x2a, 0xb5, 0x00, 0x07, 0x2a, 0xb2, 0x00, 0x0d, 0xb5, 0x00,
    0x11, 0x2a, 0xb2, 0x00, 0x14, 0xb5, 0x00, 0x18, 0x2a, 0xb6, 0x00, 0x1b, 0x57, 0x2a, 0x2b,
    0xb5, 0x00, 0x2d, 0xb2, 0x00, 0x1f, 0x99, 0x00, 0x0b, 0x2a, 0xb6, 0x00, 0x23, 0x2b, 0xb8,
    0x00, 0x27, 0xb1,
];
const JDK25_THROWABLE_INIT_EMPTY: [u8; 43] = [
    0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x2a, 0xb5, 0x00, 0x07, 0x2a, 0xb2, 0x00, 0x0d, 0xb5, 0x00,
    0x11, 0x2a, 0xb2, 0x00, 0x14, 0xb5, 0x00, 0x18, 0x2a, 0xb6, 0x00, 0x1b, 0x57, 0xb2, 0x00,
    0x1f, 0x99, 0x00, 0x0b, 0x2a, 0xb6, 0x00, 0x23, 0x01, 0xb8, 0x00, 0x27, 0xb1,
];
const JDK25_THROWABLE_FILL: [u8; 29] = [
    0x2a, 0xb4, 0x00, 0x11, 0xc7, 0x00, 0x0a, 0x2a, 0xb4, 0x00, 0xc6, 0xc6, 0x00, 0x10, 0x2a,
    0x03, 0xb6, 0x00, 0xca, 0x57, 0x2a, 0xb2, 0x00, 0x0d, 0xb5, 0x00, 0x11, 0x2a, 0xb0,
];
const JDK25_NPE_FILL: [u8; 41] = [
    0x2a, 0xb4, 0x00, 0x0a, 0x9a, 0x00, 0x0b, 0x2a, 0x04, 0xb5, 0x00, 0x0a, 0xa7, 0x00, 0x18,
    0x2a, 0xb4, 0x00, 0x0a, 0x04, 0xa0, 0x00, 0x10, 0x2a, 0x2a, 0xb6, 0x00, 0x10, 0xb5, 0x00,
    0x14, 0x2a, 0x05, 0xb5, 0x00, 0x0a, 0x2a, 0xb7, 0x00, 0x18, 0xb0,
];

#[test]
fn nio21_w19_the_jdk25_chain_matches_its_shapes() {
    assert!(code_matches_shape(
        &JDK25_THROWABLE_INIT_STRING,
        THROWABLE_INIT_STRING
    ));
    assert!(code_matches_shape(
        &JDK25_THROWABLE_INIT_EMPTY,
        THROWABLE_INIT_EMPTY
    ));
    assert!(code_matches_shape(
        &JDK25_THROWABLE_FILL,
        THROWABLE_FILL_IN_STACK_TRACE
    ));
    assert!(code_matches_shape(&JDK25_NPE_FILL, NPE_FILL_IN_STACK_TRACE));
    assert!(code_matches_shape(
        &[0x2a, 0x2b, 0xb7, 0x00, 0x07, 0xb1],
        DELEGATE_INIT_STRING
    ));
    assert!(code_matches_shape(
        &[0x2a, 0xb7, 0x00, 0x01, 0xb1],
        DELEGATE_INIT_EMPTY
    ));
}

/// An instrumented or different constructor refuses: one inserted call, a
/// moved branch, a different store, a wrong length.
#[test]
fn nio21_w19_any_other_bytecode_refuses() {
    let mut moved = JDK25_THROWABLE_INIT_STRING;
    moved[38] = 0x0c; // ifeq +12
    assert!(!code_matches_shape(&moved, THROWABLE_INIT_STRING));
    let mut other_store = JDK25_THROWABLE_INIT_STRING;
    other_store[30] = 0xb3; // putstatic instead of putfield
    assert!(!code_matches_shape(&other_store, THROWABLE_INIT_STRING));
    let mut instrumented = vec![0xb8, 0x00, 0x99];
    instrumented.extend_from_slice(&JDK25_THROWABLE_INIT_STRING);
    assert!(!code_matches_shape(&instrumented, THROWABLE_INIT_STRING));
    // A delegation that loads a constant message instead of its argument.
    assert!(!code_matches_shape(
        &[0x2a, 0x12, 0xb7, 0x00, 0x07, 0xb1],
        DELEGATE_INIT_STRING
    ));
    assert!(!code_matches_shape(
        &JDK25_THROWABLE_INIT_EMPTY,
        THROWABLE_INIT_STRING
    ));
}

/// The direct build is wired into the trap-frame door and (JIT round 23
/// wave 1, lane L3) into `create_exception_object` behind its switch; the
/// trap door's refusal arm runs the constructors without asking a second
/// time (a source pin).
#[test]
fn nio21_w19_only_the_trap_frame_door_builds_directly() {
    let src = include_str!("../exceptions.rs");
    let needle = format!(
        "match {}(shared, thread, class_name, message)",
        "try_build_implicit_throwable_directly"
    );
    let calls = src.matches(needle.as_str()).count();
    assert_eq!(calls, 1);
    let trap = src_fn("pub(crate) fn create_exception_object_with_trap_frames_live(");
    let args = "(shared, thread, class_name, message)";
    assert!(trap.contains(&format!("create_exception_object_constructed{args}")));
    assert!(!trap.contains(&format!("create_exception_object{args}")));
    let door = src_fn("pub fn create_exception_object(");
    let gate = door
        .find("direct_build_every_door_enabled()")
        .expect("the every-door switch");
    let ask = door
        .find("try_build_implicit_throwable_directly(shared, thread, class_name, message)")
        .expect("asks the direct build");
    let collect = door
        .find("collect_if_requested(shared, thread)")
        .expect("takes the collection request first");
    let fallback = door
        .find("create_exception_object_constructed(shared, thread, class_name, message)")
        .expect("the constructors on a refusal");
    assert!(gate < collect && collect < ask && ask < fallback);
}

/// nio21 wave 20 L1: the dispatch helper reaches the throw-new direct
/// build exactly once, behind the `<init>` screen and after the
/// null-receiver NPE (a null receiver must keep raising at the invoke).
#[test]
fn nio21_w20_the_dispatch_helper_builds_throw_new_after_the_null_receiver_check() {
    let helpers = crate::jit::helpers::HELPERS_SRC;
    let body = helpers
        .split("unsafe fn jit_invoke_dispatch_body_inner(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("jit_invoke_dispatch_body");
    let hook = body
        .find("try_throwable_init_direct_at_dispatch(vm, info, args_slice)")
        .expect("the hook");
    let npe = body
        .find("jit_action::INVOKE_RECEIVER")
        .expect("the null-receiver NPE");
    assert!(npe < hook);
    assert_eq!(
        body.matches("try_throwable_init_direct_at_dispatch(")
            .count(),
        1
    );
    // The receiver must be exactly the named class.
    let direct = src_fn("pub(crate) fn jit_new_throwable_direct(");
    assert!(direct.contains("&*class.name != class_name"));
    assert!(direct.contains("ClassLoaderId::Bootstrap"));
}

/// nio21 wave 21 L1: every capture (the interpreted constructors' through
/// `fillInStackTrace(0)`, the native shadows', the direct builds') pins its
/// throwable, reads the pin back and captures again at a moved address,
/// under the switch; the pin is released on every path.
#[test]
fn nio21_w21_every_capture_recaptures_a_moved_throwable() {
    let exec = concat!(include_str!("../../vm/vm_exec/mod.rs"), "\n", include_str!("../../vm/vm_exec/method.rs"), "\n", include_str!("../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../vm/vm_exec/jni_table_native_call.rs"));
    let wrapper = exec
        .split(
            "    fn capture_throwable_stack_trace(&mut self, throwable: ObjectRef) -> usize {",
        )
        .nth(1)
        .and_then(|rest| rest.split(concat!("\n", "    }", "\n")).next())
        .expect("the capture wrapper");
    let pin = wrapper
        .find("native_pin_roots.push(throwable)")
        .expect("pins the throwable");
    // After the pin: the switch-off arm above it captures once too.
    let once = pin
        + wrapper[pin..]
            .find("capture_throwable_stack_trace_once(throwable)")
            .expect("captures once");
    let again = wrapper
        .find("capture_throwable_stack_trace_once(now)")
        .expect("captures again at the moved address");
    let release = wrapper
        .find("native_pin_roots.truncate(pin)")
        .expect("releases the pin");
    assert!(pin < once && once < again && again < release);
    assert!(wrapper.contains("throwable_recapture_if_moved_enabled()"));
    assert_eq!(
        exec.matches("fn capture_throwable_stack_trace_once(")
            .count(),
        1
    );
}

/// The body of `head` in `src`, up to its first column-0 closing brace.
fn body_in<'a>(src: &'a str, head: &str) -> &'a str {
    src.split(head)
        .nth(1)
        .and_then(|rest| rest.split(concat!("\n", "}", "\n")).next())
        .expect("function body")
}

/// nio21 wave 21 L1: a handler-bearing lambda impl reaches the compiled
/// site only under its switch, and both services that can see a throwable
/// leave it route that throwable through the impl's table: the Rust
/// direct arm BEFORE it restashes an explicit throwable as "not a deopt",
/// and the callee-deopt service BEFORE its receiver-based table probe
/// (which cannot see a proxy's impl), never at an owned-arguments site.
#[test]
fn nio21_w21_handler_bearing_lambda_impls_route_through_their_own_table() {
    let lambda = include_str!("../interpreter/lambda.rs");
    let site = body_in(lambda, "pub(crate) fn lambda_jit_site(");
    assert!(site.contains("lambda_handler_impl_enabled()"));
    assert!(site.contains("build_lambda_jit_site(shared, proxy_class_id, allow_handlers)"));
    let helpers = crate::jit::helpers::HELPERS_SRC;
    let direct = body_in(helpers, "unsafe fn try_lambda_site_direct_call(");
    let routed = direct
        .find("lambda_route_thrown_through_impl(")
        .expect("the direct arm routes an explicit throwable");
    let restash = direct
        .find("if lambda_direct_sentinel_is_not_a_deopt(")
        .expect("the not-a-deopt restash");
    assert!(routed < restash);
    let service = body_in(
        helpers,
        "unsafe fn handle_compiled_callee_deopt_sentinel_inner(",
    );
    let hook = service
        .find("lambda_handler_site_for_service(")
        .expect("the service hook");
    let probe = service
        .find("mic_callee_has_exception_table(vm, receiver_class_id, info)")
        .expect("the receiver-based probe");
    assert!(hook < probe);
    assert!(service.contains("if !args_owned_by_callee && !(escaped_redispatch"));
    let gate = body_in(helpers, "fn lambda_handler_site_for_service(");
    assert!(gate.contains("lambda_handler_impl_enabled()"));
    assert!(gate.contains("exception_table.is_empty()"));
    // W4A item 1: the instance site admits a handler-bearing impl under the
    // same switch, and the service finds it by the receiver's class.
    let instance = body_in(lambda, "fn build_lambda_instance_jit_site(");
    assert!(instance.contains("lambda_handler_impl_enabled()"));
    assert!(gate.contains("lambda_instance_jit_site("));
}

fn src_fn(head: &str) -> String {
    include_str!("../exceptions.rs")
        .split(head)
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("function body")
        .to_string()
}
