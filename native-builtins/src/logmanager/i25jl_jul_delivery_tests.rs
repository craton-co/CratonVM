use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::{Cell, RefCell};

/// Forgets the VM's logging rows however the test ends.
struct ForgetVm(usize);
impl Drop for ForgetVm {
    fn drop(&mut self) {
        forget_vm_logging_state(self.0);
    }
}

thread_local! {
    /// `(list address, its handlers)` the hook answers `size` / `get` from.
    static LISTS: RefCell<Vec<(usize, Vec<ObjectRef>)>> = const { RefCell::new(Vec::new()) };
    /// Receivers of every `Handler.publish`, in call order.
    static PUBLISHED: RefCell<Vec<ObjectRef>> = const { RefCell::new(Vec::new()) };
    /// What `Filter.isLoggable(LogRecord)` answers, and how often it ran.
    static FILTER_ANSWER: Cell<i32> = const { Cell::new(1) };
    static FILTER_CALLS: Cell<usize> = const { Cell::new(0) };
}

fn reset_hook_state() {
    LISTS.with(|l| l.borrow_mut().clear());
    PUBLISHED.with(|p| p.borrow_mut().clear());
    FILTER_ANSWER.with(|a| a.set(1));
    FILTER_CALLS.with(|c| c.set(0));
}

fn published() -> Vec<ObjectRef> {
    PUBLISHED.with(|p| p.borrow().clone())
}

fn delivery_hook(
    _ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    let key = receiver.as_ptr() as usize;
    match (method_name, descriptor) {
        ("isLoggable", "(Ljava/util/logging/LogRecord;)Z") => {
            FILTER_CALLS.with(|c| c.set(c.get() + 1));
            Some(Ok(Some(Value::Int(FILTER_ANSWER.with(Cell::get)))))
        }
        ("size", "()I") => LISTS.with(|l| {
            l.borrow()
                .iter()
                .find(|(list, _)| *list == key)
                .map(|(_, handlers)| Ok(Some(Value::Int(handlers.len() as i32))))
        }),
        ("get", "(I)Ljava/lang/Object;") => {
            let Some(Value::Int(index)) = args.first() else {
                return None;
            };
            let index = *index as usize;
            LISTS.with(|l| {
                l.borrow()
                    .iter()
                    .find(|(list, _)| *list == key)
                    .and_then(|(_, handlers)| handlers.get(index).copied())
                    .map(|h| Ok(Some(Value::Object(Some(h)))))
            })
        }
        ("publish", "(Ljava/util/logging/LogRecord;)V") => {
            PUBLISHED.with(|p| p.borrow_mut().push(receiver));
            Some(Ok(None))
        }
        _ => None,
    }
}

/// Record `useParentHandlers == true` for each logger, as HotSpot's
/// default.
///
/// MOCK ARTIFACT, not a product default (i25a hand-back): the mock
/// resolves `Logger.config` through the synthetic stub field model, so
/// `allocate_logger`'s `populate_real_logger_config` installs a
/// `Logger$ConfigurationData` whose constructor never ran; its
/// `useParentHandlers` reads the mock's absent-field `Int(0)`, i.e.
/// `false`, and `jul_use_parent_handlers` stopped every walk at the
/// logger itself. In the VM the real constructor sets it to `true`. The
/// setter's side table is consulted first, so this states the JDK default
/// explicitly.
fn parents_on(ctx: &MockNativeContext, loggers: &[ObjectRef]) {
    for &logger in loggers {
        crate::jul_logger_set_use_parent_handlers(ctx, logger, true);
    }
}

/// Give `logger` an identity-keyed handler list holding `handlers`.
fn give_handlers(ctx: &mut MockNativeContext, logger: ObjectRef, handlers: &[ObjectRef]) {
    let list = ctx.fresh_object_ref();
    crate::jul_logger_handlers_set(ctx, logger, list);
    LISTS.with(|l| {
        l.borrow_mut()
            .push((list.as_ptr() as usize, handlers.to_vec()))
    });
}

/// A `Level` the threshold walk reads (the `tests::make_level` recipe).
fn level(ctx: &mut MockNativeContext, name: &str, value: i32) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(CLS_JUL_LEVEL).unwrap();
    ctx.set_declared_fields(
        cid,
        vec![
            cratonvm_native_api::FieldMetadata {
                name: "name".to_string(),
                descriptor: "Ljava/lang/String;".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: cid,
                is_static: false,
            },
            cratonvm_native_api::FieldMetadata {
                name: "value".to_string(),
                descriptor: "I".to_string(),
                access_flags: 0,
                slot_index: 1,
                declaring_class_id: cid,
                is_static: false,
            },
        ],
    );
    let obj = ctx.alloc_object(cid, 2);
    let n = ctx.create_string(name);
    ctx.set_field(obj, 0, Value::Object(Some(n)));
    ctx.set_field(obj, 1, Value::Int(value));
    ctx.set_field_by_name(obj, "name", Value::Object(Some(n)));
    ctx.set_field_by_name(obj, "value", Value::Int(value));
    obj
}

#[test]
fn gci_i25jl_only_an_explicit_false_from_the_filter_rejects() {
    assert!(!jul_filter_answer_admits(Some(Value::Int(0))));
    assert!(jul_filter_answer_admits(Some(Value::Int(1))));
    assert!(
        jul_filter_answer_admits(None),
        "a void-shaped answer (no filter body ran) must not drop the record"
    );
    assert!(JulPublish::Filtered.disposed(), "a filtered record prints nothing");
    assert!(JulPublish::Delivered.disposed());
    assert!(!JulPublish::NoHandler.disposed(), "only an untaken record falls back");
}

#[test]
fn gci_i25jl_an_off_logger_admits_nothing_not_even_off() {
    assert!(!jul_level_passes_threshold(i32::MAX, i32::MAX), "OFF on an OFF logger");
    assert!(!jul_level_passes_threshold(1000, i32::MAX), "SEVERE on an OFF logger");
    assert!(jul_level_passes_threshold(1000, 800));
    assert!(jul_level_passes_threshold(800, 800));
    assert!(!jul_level_passes_threshold(500, 800), "FINE on an INFO logger");
    assert!(jul_level_passes_threshold(300, i32::MIN), "FINEST on an ALL logger");
}

#[test]
fn gci_i25jl_remove_handler_drops_only_the_receivers_own_row() {
    // Two same-named objects (owners 1 and 2) both filed handler 10.
    let mut rows: Vec<NamedHandlerRow> = vec![(1, 10), (2, 10), (1, 11)];
    remove_named_handler_row(&mut rows, 2, 10);
    assert_eq!(rows, vec![(1, 10), (1, 11)], "b.removeHandler(h) must not take h off a");
    remove_named_handler_row(&mut rows, 1, 10);
    assert_eq!(rows, vec![(1, 11)]);
    remove_named_handler_row(&mut rows, 1, 99);
    assert_eq!(rows, vec![(1, 11)], "a handler the owner never added: no change");
}

#[test]
fn gci_i25jl_the_chain_reaches_every_level_until_use_parent_handlers_stops_it() {
    const VM: usize = 0xE25_1101;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let root = get_or_create_logger(&mut ctx, "").unwrap();
    let a = get_or_create_logger(&mut ctx, "gcijl25c.a").unwrap();
    let ab = get_or_create_logger(&mut ctx, "gcijl25c.a.b").unwrap();
    let abc = get_or_create_logger(&mut ctx, "gcijl25c.a.b.c").unwrap();
    parents_on(&ctx, &[root, a, ab, abc]);
    let l_root = ctx.fresh_object_ref();
    let l_a = ctx.fresh_object_ref();
    let l_ab = ctx.fresh_object_ref();
    crate::jul_logger_handlers_set(&mut ctx, root, l_root);
    crate::jul_logger_handlers_set(&mut ctx, a, l_a);
    crate::jul_logger_handlers_set(&mut ctx, ab, l_ab);

    assert_eq!(
        resolve_jul_handler_chain(&mut ctx, ab),
        vec![l_ab, l_a, l_root],
        "an own handler list no longer ends the walk (F2)"
    );
    assert_eq!(
        resolve_jul_handler_chain(&mut ctx, abc),
        vec![l_ab, l_a, l_root],
        "a logger with no list of its own starts at its nearest ancestor"
    );
    crate::jul_logger_set_use_parent_handlers(&ctx, a, false);
    assert_eq!(
        resolve_jul_handler_chain(&mut ctx, ab),
        vec![l_ab, l_a],
        "an ANCESTOR's useParentHandlers=false stops the walk after it"
    );
    crate::jul_logger_set_use_parent_handlers(&ctx, ab, false);
    assert_eq!(resolve_jul_handler_chain(&mut ctx, ab), vec![l_ab]);
    assert_eq!(
        resolve_jul_handler_chain(&mut ctx, abc),
        vec![l_ab],
        "the level that decides is the one visited"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// gci i26/rv (item J1 of
/// `gci-i26rv-i25-default-path-review-findings-FIXED-20261016`): a walk stopped
/// below the root by `useParentHandlers == false` never reaches the root's
/// `ConsoleHandler`, so an untaken record is disposed of silently
/// (`Silenced`), not printed by the console sink.
#[test]
fn gci_i26rv_a_walk_stopped_below_the_root_prints_no_console_line() {
    const VM: usize = 0xE26_1101;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let root = get_or_create_logger(&mut ctx, "").unwrap();
    let q = get_or_create_logger(&mut ctx, "gcirv26.q").unwrap();
    let qr = get_or_create_logger(&mut ctx, "gcirv26.q.r").unwrap();
    parents_on(&ctx, &[root, q, qr]);

    assert_eq!(
        resolve_jul_handler_chain_and_stop(&mut ctx, qr),
        (Vec::new(), false),
        "nothing stops the walk: the console sink stands for the root's handler"
    );
    crate::jul_logger_set_use_parent_handlers(&ctx, q, false);
    assert_eq!(
        resolve_jul_handler_chain_and_stop(&mut ctx, q),
        (Vec::new(), true),
        "setUseParentHandlers(false) with no handler: silenced"
    );
    assert_eq!(
        resolve_jul_handler_chain_and_stop(&mut ctx, qr),
        (Vec::new(), true),
        "an ancestor below the root stops it too"
    );
    // The publish reports it: Silenced (disposed, no console) for the
    // stopped walk, NoHandler (console sink) for one that reaches the root.
    let info = level(&mut ctx, "INFO", 800);
    let msg = ctx.create_string("m");
    assert_eq!(
        publish_to_jul_handlers_outcome(&mut ctx, qr, info, Some(msg), None, None, None, None)
            .unwrap(),
        JulPublish::Silenced
    );
    let w = get_or_create_logger(&mut ctx, "gcirv26.w").unwrap();
    parents_on(&ctx, &[w]);
    assert_eq!(
        publish_to_jul_handlers_outcome(&mut ctx, w, info, Some(msg), None, None, None, None)
            .unwrap(),
        JulPublish::NoHandler
    );
    let l_q = ctx.fresh_object_ref();
    crate::jul_logger_handlers_set(&mut ctx, q, l_q);
    assert_eq!(resolve_jul_handler_chain_and_stop(&mut ctx, qr), (vec![l_q], true));
    crate::jul_logger_set_use_parent_handlers(&ctx, root, false);
    assert_eq!(
        resolve_jul_handler_chain_and_stop(&mut ctx, root),
        (Vec::new(), false),
        "the root's own flag stops nothing: it has no parent"
    );
    assert!(jul_walk_stopped_below_root("a.b"));
    assert!(!jul_walk_stopped_below_root(""));
    assert!(JulPublish::Silenced.disposed(), "a silenced record prints nothing");
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn gci_i25jl_a_null_message_is_published_to_every_level_and_the_level_gates() {
    const VM: usize = 0xE25_1102;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    reset_hook_state();
    ctx.set_invoke_virtual_hook(delivery_hook);
    let root = get_or_create_logger(&mut ctx, "").unwrap();
    let a = get_or_create_logger(&mut ctx, "gcijl25n.a").unwrap();
    parents_on(&ctx, &[root, a]);
    let h_root = ctx.fresh_object_ref();
    let h_a = ctx.fresh_object_ref();
    give_handlers(&mut ctx, root, &[h_root]);
    give_handlers(&mut ctx, a, &[h_a]);
    let severe = level(&mut ctx, "SEVERE", 1000);

    // log(SEVERE, (String) null): HotSpot publishes `new LogRecord(level,
    // null)` to a's handler and then the root's (i25/rv item 14 + F2).
    native_jul_logger_log_level_msg(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Object(Some(severe)),
            Value::Object(None),
        ],
    )
    .unwrap();
    assert_eq!(published(), vec![h_a, h_root]);

    // A FINE record on an INFO logger: HotSpot's `isLoggable` gate drops it
    // before any handler (this overload never gated).
    PUBLISHED.with(|p| p.borrow_mut().clear());
    let fine = level(&mut ctx, "FINE", 500);
    let msg = ctx.create_string("quiet");
    native_jul_logger_log_level_msg(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Object(Some(fine)),
            Value::Object(Some(msg)),
        ],
    )
    .unwrap();
    assert!(published().is_empty(), "gated by the level");
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin is released");
}

#[test]
fn gci_i25jl_a_rejecting_filter_is_asked_once_and_stops_every_delivery() {
    const VM: usize = 0xE25_1103;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    reset_hook_state();
    ctx.set_invoke_virtual_hook(delivery_hook);
    let root = get_or_create_logger(&mut ctx, "").unwrap();
    let a = get_or_create_logger(&mut ctx, "gcijl25f.a").unwrap();
    parents_on(&ctx, &[root, a]);
    let h_root = ctx.fresh_object_ref();
    let h_a = ctx.fresh_object_ref();
    give_handlers(&mut ctx, root, &[h_root]);
    give_handlers(&mut ctx, a, &[h_a]);
    let filter = ctx.fresh_object_ref();
    crate::jul_logger_filter_set(&mut ctx, a, Some(filter));
    let info = level(&mut ctx, "INFO", 800);
    let msg = ctx.create_string("m");
    let args = [
        Value::Object(Some(a)),
        Value::Object(Some(info)),
        Value::Object(Some(msg)),
    ];

    FILTER_ANSWER.with(|f| f.set(0));
    native_jul_logger_log_level_msg(&mut ctx, &args).unwrap();
    assert_eq!(FILTER_CALLS.with(Cell::get), 1, "asked once, with the handlers' record");
    assert!(published().is_empty(), "a false answer stops delivery (F1)");

    FILTER_ANSWER.with(|f| f.set(1));
    native_jul_logger_log_level_msg(&mut ctx, &args).unwrap();
    assert_eq!(FILTER_CALLS.with(Cell::get), 2);
    assert_eq!(published(), vec![h_a, h_root]);

    // `Logger.log(LogRecord)`: the same filter gate on an existing record.
    PUBLISHED.with(|p| p.borrow_mut().clear());
    FILTER_ANSWER.with(|f| f.set(0));
    let record = ctx.fresh_object_ref();
    assert!(
        publish_existing_record_to_jul_handlers(&mut ctx, a, record).unwrap(),
        "a filtered record is disposed of: no console fallback"
    );
    assert!(published().is_empty());
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin is released");
}

#[test]
fn gci_i25jl_the_weak_registry_holds_named_loggers_by_handle_and_root_and_global_strongly() {
    const VM: usize = 0xE25_1104;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let a = get_or_create_logger_with_rows(&mut ctx, "gcijl25w.a", true).unwrap();
    let row = registry_row_value(VM, "gcijl25w.a");
    assert!(registry_row_is_handle(row), "a named logger's row is a handle");
    assert_eq!(registry_row_logger(&ctx, row), Some(a));
    let root_row = registry_row_value(VM, "");
    assert!(root_row != 0 && !registry_row_is_handle(root_row), "the root is strong");
    let root = registry_row_logger(&ctx, root_row).unwrap();

    let mut roots = Vec::new();
    gc_scan_logmanager_roots(VM, &mut roots);
    assert!(!roots.contains(&a), "a handle row is no root");
    assert!(roots.contains(&root));

    let again = get_or_create_logger_with_rows(&mut ctx, "gcijl25w.a", true).unwrap();
    assert_eq!(again, a);
    assert_eq!(existing_loggers_for_name(&ctx, "gcijl25w.a")[1], Some(a));
    assert_eq!(nearest_existing_ancestor_name(&ctx, "gcijl25w.a.x"), "gcijl25w.a");

    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(row as usize, 0x1000);
    gc_update_logmanager_refs(VM, &moved);
    assert_eq!(registry_row_value(VM, "gcijl25w.a"), row, "a handle row is never remapped");

    // A collection found the Logger dead: its handle answers None.
    assert!(ctx.remove_global_root(registry_row_handle(row)));
    assert_eq!(existing_loggers_for_name(&ctx, "gcijl25w.a")[1], None);
    assert_eq!(
        nearest_existing_ancestor_name(&ctx, "gcijl25w.a.x"),
        "",
        "a cleared row is not a parent"
    );
    let b = get_or_create_logger_with_rows(&mut ctx, "gcijl25w.a", true).unwrap();
    assert_ne!(b, a, "the name is minted afresh (LogManager.demandLogger)");
    let row_b = registry_row_value(VM, "gcijl25w.a");
    assert!(registry_row_is_handle(row_b) && row_b != row);

    get_or_create_logger_with_rows(&mut ctx, "global", true).unwrap();
    let global_row = registry_row_value(VM, "global");
    assert!(global_row != 0 && !registry_row_is_handle(global_row), "global is strong");
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin is released");
}

#[test]
fn gci_i25jl_a_new_weak_node_reparents_the_live_descendants_it_stands_over() {
    const VM: usize = 0xE25_1105;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let deep = get_or_create_logger_with_rows(&mut ctx, "gcijl25r.a.b.c", true).unwrap();
    let mid = get_or_create_logger_with_rows(&mut ctx, "gcijl25r.a", true).unwrap();
    assert!(
        matches!(ctx.get_field(deep, LOGGER_FIELD_PARENT), Value::Object(Some(p)) if p == mid),
        "LogNode.walkAndSetParent: the new nearest ancestor"
    );
    let between = get_or_create_logger_with_rows(&mut ctx, "gcijl25r.a.b", true).unwrap();
    assert!(matches!(
        ctx.get_field(deep, LOGGER_FIELD_PARENT),
        Value::Object(Some(p)) if p == between
    ));
    assert!(matches!(
        ctx.get_field(between, LOGGER_FIELD_PARENT),
        Value::Object(Some(p)) if p == mid
    ));
}

thread_local! {
    /// gci i32/jh: the handler whose `publish` throws (address; 0 none).
    static THROWING: Cell<usize> = const { Cell::new(0) };
}

/// [`delivery_hook`], except the [`THROWING`] handler's `publish` fails
/// with a Java exception (the handler object itself stands in for it).
fn throwing_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "publish"
        && descriptor == "(Ljava/util/logging/LogRecord;)V"
        && receiver.as_ptr() as usize == THROWING.with(Cell::get)
    {
        PUBLISHED.with(|p| p.borrow_mut().push(receiver));
        return Some(Err(MethodCallFailed::ExceptionThrown(receiver)));
    }
    delivery_hook(ctx, receiver, method_name, descriptor, args)
}

/// gci i32/jh (`i31-hp-report.md` review item 5): HotSpot's
/// `Logger.log(LogRecord)` has no catch around `handler.publish`, so a
/// handler's exception leaves the logging call and the handlers after it
/// (here the root's) are not offered the record; the parameterized door
/// used to swallow it into the console sink.
#[test]
fn gci_i32jh_a_throwing_handler_fails_the_publish() {
    const VM: usize = 0xE32_1101;
    let _forget = ForgetVm(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    reset_hook_state();
    ctx.set_invoke_virtual_hook(throwing_hook);
    let root = get_or_create_logger(&mut ctx, "").unwrap();
    let a = get_or_create_logger(&mut ctx, "gcijh32.a").unwrap();
    parents_on(&ctx, &[root, a]);
    let h_root = ctx.fresh_object_ref();
    let h_a = ctx.fresh_object_ref();
    give_handlers(&mut ctx, root, &[h_root]);
    give_handlers(&mut ctx, a, &[h_a]);
    THROWING.with(|t| t.set(h_a.as_ptr() as usize));
    let info = level(&mut ctx, "INFO", 800);
    let msg = ctx.create_string("m");

    let outcome =
        publish_to_jul_handlers_outcome(&mut ctx, a, info, Some(msg), None, None, None, None);
    assert!(
        matches!(outcome, Err(MethodCallFailed::ExceptionThrown(t)) if t == h_a),
        "the handler's exception is the call's"
    );
    assert_eq!(published(), vec![h_a], "the root's handler is not offered it");

    // `log(Level, String, Object[])`'s door propagates it too (it fell to
    // the console sink).
    PUBLISHED.with(|p| p.borrow_mut().clear());
    let r = jul_log_parameterized(&mut ctx, Some(a), Some(info), Some(msg), None, None);
    assert!(matches!(r, Err(MethodCallFailed::ExceptionThrown(t)) if t == h_a));
    assert_eq!(published(), vec![h_a]);

    // No throwing handler: both handlers take it, as before.
    THROWING.with(|t| t.set(0));
    PUBLISHED.with(|p| p.borrow_mut().clear());
    assert_eq!(
        publish_to_jul_handlers_outcome(&mut ctx, a, info, Some(msg), None, None, None, None)
            .unwrap(),
        JulPublish::Delivered
    );
    assert_eq!(published(), vec![h_a, h_root]);
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin is released");
}
