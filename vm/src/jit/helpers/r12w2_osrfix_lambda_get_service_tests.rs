use super::*;
use crate::config::VmConfig;

/// The text of the production function whose definition starts with `sig`.
fn production_fn(src: &str, sig: &str) -> String {
    let start = src.find(sig);
    assert!(start.is_some(), "`{sig}` not found");
    let body = &src[start.unwrap_or(0)..];
    body[..body.find("\n}\n").map_or(body.len(), |e| e + 3)].to_string()
}

/// The service site dispatches on the receiver, roots only the receiver,
/// claims a stash of the getter's own frame, and its resume twin carries
/// the receiver's class as the loader context and nothing else new.
#[test]
fn the_direct_get_site_dispatches_on_the_receiver_and_pins_only_it() {
    let info = &LAMBDA_GET_DIRECT_INFO;
    assert!(invoke_site_has_receiver(info.invoke_kind));
    assert!(
        matches!(info.invoke_kind, 0 | 2),
        "resolved from the receiver's class"
    );
    assert_eq!(
        info.declaring_class_id, 0,
        "the route already asked selection"
    );
    let mut pinned = Vec::new();
    for_each_jit_reference_arg(info, &[0x1000, 7], |index, _| pinned.push(index));
    assert_eq!(pinned, vec![0], "the receiver, not the int index");
    assert!(exceptional_frame_key_names(
        "app/Thrower.get:(I)D",
        info.method_name,
        info.descriptor
    ));
    assert!(!exceptional_frame_key_names(
        "app/Loop.loop:(Ljava/util/function/Function;I)D",
        info.method_name,
        info.descriptor
    ));
    let site = lambda_get_resume_site(ClassId::new(77));
    assert_eq!(site.declaring_class_id, 77);
    assert_eq!(site.owner_class_id, 0);
    assert_eq!(
        (site.method_name, site.descriptor, site.invoke_kind),
        (info.method_name, info.descriptor, info.invoke_kind)
    );
    assert_eq!(
        (site.num_jit_args, site.return_type),
        (info.num_jit_args, info.return_type)
    );
}

/// A value, and a bare `i64::MIN` with no signal beside it (a `get` that
/// returned `-0.0`), come back unchanged; no call is no answer.
#[test]
fn a_value_or_a_bare_negative_zero_passes_through_unchanged() {
    let vm = SharedVm::new(VmConfig::default());
    let _ = take_jit_deopt_pending();
    let args = [0i64, 3];
    // SAFETY: no JIT thread is installed on this test thread, so no
    // `jit_thread_mut` borrow is live, and `call` enters no compiled code.
    let (plain, neg_zero, none) = unsafe {
        (
            lambda_get_call_then_route(&vm, ClassId::new(0), &args, || Some(42)),
            lambda_get_call_then_route(&vm, ClassId::new(0), &args, || Some(i64::MIN)),
            lambda_get_call_then_route(&vm, ClassId::new(0), &args, || None),
        )
    };
    assert_eq!(plain, Some(42));
    assert_eq!(neg_zero, Some(i64::MIN));
    assert_eq!(none, None);
}

/// The peephole enters its compiled `get` only inside the service, and the
/// service resumes a pure deopt before the general routing. A text pin:
/// the helper cannot be driven without compiled code. Before the fix the
/// body called the entry directly and returned its raw result.
#[test]
fn the_peephole_enters_compiled_get_only_under_the_service() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = production_fn(&src, "\npub unsafe fn jit_lambda_int_to_double_body(");
    let service = body.find("lambda_get_call_then_route(");
    let entry = body.find("try_call_compiled_entry_reentrant(");
    assert!(
        matches!((service, entry), (Some(s), Some(e)) if s < e),
        "the compiled `get` is entered inside the service's closure"
    );
    assert_eq!(
        body.matches("try_call_compiled_entry_reentrant(").count(),
        1
    );
    let route = production_fn(&src, "\npub unsafe fn lambda_get_call_then_route(");
    let resume = route.find("resume_trapped_lambda_get(");
    let general = route.find("route_implicit_exc_through_callee(");
    assert!(
        matches!((resume, general), (Some(r), Some(g)) if r < g),
        "a pure deopt is resumed before the general routing"
    );
}
