use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// The lists' element component (any id but the untyped `0`).
fn element_component() -> cratonvm_types::ClassId {
    cratonvm_types::ClassId::new(1)
}

thread_local! {
    static ADDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn declare(ctx: &mut MockNativeContext, class_name: &str, names: &[&str]) {
    let cid = ctx.ensure_class_initialized(class_name).expect("class");
    let fields = names
        .iter()
        .enumerate()
        .map(|(i, n)| cratonvm_native_api::FieldMetadata {
            name: (*n).to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: i,
            declaring_class_id: cid,
            is_static: false,
        })
        .collect();
    ctx.set_declared_fields(cid, fields);
}

fn object(ctx: &mut MockNativeContext, class_name: &str) -> ObjectRef {
    match ctx.new_object(class_name).unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected {class_name} object, got {other:?}"),
    }
}

/// A loader whose `classes` list holds `size` elements in an
/// `elementData` of `capacity` slots. zt t7/x4: the classes are the JDK's
/// own names, `java/lang/ClassLoader` (whose `classes` slot the presize
/// resolves) and `java/util/ArrayList` (the only list class it touches).
fn loader_with_list(
    ctx: &mut MockNativeContext,
    size: usize,
    capacity: usize,
) -> (ObjectRef, ObjectRef) {
    declare(ctx, CL_CLASS, &["classes"]);
    let loader = object(ctx, CL_CLASS);
    let list = list_of(ctx, ARRAY_LIST_CLASS, size, capacity);
    ctx.set_field_by_name(loader, "classes", Value::Object(Some(list)));
    ADDS.with(|c| c.set(0));
    (loader, list)
}

/// A list object of `class_name` with `size` elements in an
/// `elementData` of `capacity` slots.
fn list_of(
    ctx: &mut MockNativeContext,
    class_name: &str,
    size: usize,
    capacity: usize,
) -> ObjectRef {
    declare(ctx, class_name, &["elementData", "size"]);
    let list = object(ctx, class_name);
    let data = ctx.new_ref_array(element_component(), capacity);
    for i in 0..size {
        let element = object(ctx, "example/Zt6Nd4Element");
        ctx.set_array_element(data, i, Value::Object(Some(element)));
    }
    ctx.set_field_by_name(list, "elementData", Value::Object(Some(data)));
    ctx.set_field_by_name(list, "size", Value::Int(size as i32));
    list
}

/// `ArrayList.add(Object)`: grow by the JDK's policy when full, store,
/// bump `size`.
fn array_list_add(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "add" || descriptor != "(Ljava/lang/Object;)Z" {
        return None;
    }
    ADDS.with(|c| c.set(c.get() + 1));
    let size = match ctx.get_field_by_name(receiver, "size") {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let old = match ctx.get_field_by_name(receiver, "elementData") {
        Value::Object(Some(a)) => a,
        other => panic!("elementData: {other:?}"),
    };
    let capacity = ctx.array_length(old);
    let data = if size == capacity {
        let grown =
            ctx.new_ref_array(element_component(), (capacity + (capacity >> 1)).max(10));
        for i in 0..size {
            let v = ctx.get_array_element(old, i);
            ctx.set_array_element(grown, i, v);
        }
        ctx.set_field_by_name(receiver, "elementData", Value::Object(Some(grown)));
        grown
    } else {
        old
    };
    ctx.set_array_element(data, size, args[0]);
    ctx.set_field_by_name(receiver, "size", Value::Int(size as i32 + 1));
    Some(Ok(Some(Value::Int(1))))
}

/// `ArrayList.add(Object)` on a spent heap: the growth throws.
fn array_list_add_refused(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "add" {
        return None;
    }
    ADDS.with(|c| c.set(c.get() + 1));
    Some(Err(crate::util_concurrent_ext::native_heap_oom(
        "java.lang.Object[]",
    )))
}

fn capacity_of(ctx: &MockNativeContext, list: ObjectRef) -> usize {
    match ctx.get_field_by_name(list, "elementData") {
        Value::Object(Some(a)) => ctx.array_length(a),
        other => panic!("elementData: {other:?}"),
    }
}

#[test]
fn zt6nd4_a_full_list_grows_before_the_define_and_keeps_its_contents() {
    let mut ctx = MockNativeContext::new();
    let (loader, list) = loader_with_list(&mut ctx, 10, 10);
    ctx.set_invoke_virtual_hook(array_list_add);
    assert!(array_list_is_full(&ctx, list));

    presize_loader_classes(&mut ctx, loader).expect("presize");

    assert_eq!(ADDS.with(std::cell::Cell::get), 1, "one add, the growth");
    assert_eq!(
        ctx.get_field_by_name(list, "size"),
        Value::Int(10),
        "contents unchanged"
    );
    assert_eq!(
        capacity_of(&ctx, list),
        15,
        "the capacity addClass's own add would reach"
    );
    assert!(
        !array_list_is_full(&ctx, list),
        "addClass now has a free slot"
    );
    assert_eq!(ctx.monitor_depth.get(), 0, "the monitor is released");
}

/// The empty default list (`elementData` of length 0) is full too: its
/// first `add` allocates the 10-slot array.
#[test]
fn zt6nd4_the_empty_default_list_gets_its_first_array_before_the_define() {
    let mut ctx = MockNativeContext::new();
    let (loader, list) = loader_with_list(&mut ctx, 0, 0);
    ctx.set_invoke_virtual_hook(array_list_add);

    presize_loader_classes(&mut ctx, loader).expect("presize");

    assert_eq!(ADDS.with(std::cell::Cell::get), 1);
    assert_eq!(ctx.get_field_by_name(list, "size"), Value::Int(0));
    assert_eq!(capacity_of(&ctx, list), 10);
}

#[test]
fn zt6nd4_a_list_with_room_is_left_alone() {
    let mut ctx = MockNativeContext::new();
    let (loader, list) = loader_with_list(&mut ctx, 3, 10);
    ctx.set_invoke_virtual_hook(array_list_add);

    presize_loader_classes(&mut ctx, loader).expect("presize");

    assert_eq!(ADDS.with(std::cell::Cell::get), 0, "no Java call, no monitor");
    assert_eq!(ctx.get_field_by_name(list, "size"), Value::Int(3));
    assert_eq!(capacity_of(&ctx, list), 10);
    assert_eq!(ctx.monitor_depth.get(), 0);
}

/// The refusal is the define's `OutOfMemoryError`, raised before the
/// reservation, with the monitor released.
#[test]
fn zt6nd4_a_refused_growth_is_the_defines_error_and_releases_the_monitor() {
    let mut ctx = MockNativeContext::new();
    let (loader, list) = loader_with_list(&mut ctx, 10, 10);
    ctx.set_invoke_virtual_hook(array_list_add_refused);

    let err = presize_loader_classes(&mut ctx, loader).expect_err("refused growth");

    assert!(format!("{err:?}").contains("OutOfMemoryError"), "{err:?}");
    assert_eq!(ADDS.with(std::cell::Cell::get), 1);
    assert_eq!(ctx.get_field_by_name(list, "size"), Value::Int(10));
    assert_eq!(
        ctx.monitor_depth.get(),
        0,
        "the monitor is released on the error arm"
    );
}

/// A loader without the JDK's `classes` list (our synthetic layout) is
/// left alone.
#[test]
fn zt6nd4_a_loader_without_a_classes_list_is_left_alone() {
    let mut ctx = MockNativeContext::new();
    declare(&mut ctx, "example/Zt6Nd4Bare", &["parent"]);
    let loader = object(&mut ctx, "example/Zt6Nd4Bare");
    ctx.set_invoke_virtual_hook(array_list_add);
    ADDS.with(|c| c.set(0));

    presize_loader_classes(&mut ctx, loader).expect("presize");

    assert_eq!(ADDS.with(std::cell::Cell::get), 0);
}

/// zt t7/x4 (finding 1): a user loader that declares its OWN `classes`
/// field (an `ArrayList`, full) shadows `ClassLoader.classes` for a
/// by-name read. The presize grows the JDK's list (the slot `addClass`
/// uses) and leaves the user's list alone: no `add`, no capacity change.
/// Before the fix the by-name read answered the user's list, so the
/// assertions on both lists fail.
#[test]
fn zt7x4_a_user_loaders_own_classes_field_is_left_alone() {
    let mut ctx = MockNativeContext::new();
    // `ClassLoader.classes` at slot 0; the subclass re-declares `classes`
    // at slot 2 (the object carries both, as an inherited layout does).
    // gcj j2/x1: both layouts carry `nameAndId`, a real-`ClassLoader`
    // witness (`REAL_CLASSLOADER_WITNESS_FIELDS`). Keep `parallelLockMap`
    // as the inherited-layout witness too. Without these witnesses the loader
    // reads as the synthetic layout, `loader_classes_list` takes the
    // by-name read (the synthetic layout has no declared slot to prefer)
    // and the test failed on the fixture, not on the code.
    declare(&mut ctx, CL_CLASS, &["classes", "parallelLockMap", "nameAndId"]);
    declare(
        &mut ctx,
        "example/Zt7X4UserLoader",
        &["classes$jdk", "parallelLockMap", "nameAndId", "classes"],
    );
    let loader = object(&mut ctx, "example/Zt7X4UserLoader");
    assert!(
        !cl_has_synthetic_layout(&ctx, loader),
        "fixture: the real ClassLoader layout"
    );
    let jdk_list = list_of(&mut ctx, ARRAY_LIST_CLASS, 10, 10);
    let user_list = list_of(&mut ctx, ARRAY_LIST_CLASS, 10, 10);
    let jdk_slot = ctx.resolve_field_index(CL_CLASS, "classes").expect("slot");
    ctx.set_field(loader, jdk_slot, Value::Object(Some(jdk_list)));
    ctx.set_field_by_name(loader, "classes", Value::Object(Some(user_list)));
    assert_eq!(
        ctx.get_field_by_name(loader, "classes"),
        Value::Object(Some(user_list)),
        "fixture: the by-name read answers the subclass's shadow"
    );
    ADDS.with(|c| c.set(0));
    ctx.set_invoke_virtual_hook(array_list_add);

    presize_loader_classes(&mut ctx, loader).expect("presize");

    assert_eq!(ADDS.with(std::cell::Cell::get), 1, "one add");
    assert_eq!(capacity_of(&ctx, jdk_list), 15, "the JDK's list grew");
    assert_eq!(capacity_of(&ctx, user_list), 10, "the user's list is untouched");
    assert_eq!(ctx.get_field_by_name(user_list, "size"), Value::Int(10));
    assert_eq!(ctx.monitor_depth.get(), 0);
}

/// zt t7/x4 (finding 1): a full list that is not exactly a
/// `java.util.ArrayList` (a subclass may override `add` or re-declare
/// `size`) is not touched; the exact class (the control) is.
#[test]
fn zt7x4_only_an_exact_array_list_is_presized() {
    let mut ctx = MockNativeContext::new();
    let (loader, _) = loader_with_list(&mut ctx, 10, 10);
    let sub = list_of(&mut ctx, "example/Zt7X4ArrayListSubclass", 10, 10);
    ctx.set_field_by_name(loader, "classes", Value::Object(Some(sub)));
    ctx.set_invoke_virtual_hook(array_list_add);
    assert!(!array_list_is_full(&ctx, sub), "not an ArrayList: never full");

    presize_loader_classes(&mut ctx, loader).expect("presize");

    assert_eq!(ADDS.with(std::cell::Cell::get), 0, "no Java call");
    assert_eq!(capacity_of(&ctx, sub), 10);
    let exact = list_of(&mut ctx, ARRAY_LIST_CLASS, 10, 10);
    assert!(array_list_is_full(&ctx, exact), "control: the exact class");
}

/// The order the fix rests on: in `define_class_via_full` the presize
/// comes before the mirror reservation (and so before the define), and
/// `record_with_loader` (the `addClass` call) after the define.
#[test]
fn zt6nd4_the_presize_runs_before_the_reservation_in_the_definer() {
    let src = concat!(include_str!("../classloader/mod.rs"), "\n", include_str!("../classloader/closed_url_classloader_ids.rs"), "\n", include_str!("../classloader/load_class_override_memo_key.rs"), "\n", include_str!("../classloader/adopt_add_class_orphan_by_id.rs"), "\n", include_str!("../classloader/builtin_classloader_find_resource_as.rs"), "\n", include_str!("../classloader/ucl_try_define_local_class.rs"));
    let start = src
        .find("pub fn define_class_via_full(")
        .expect("definer");
    let end = start
        + src[start..]
            .find("\npub fn record_with_loader(")
            .expect("record_with_loader after the definer");
    let body = &src[start..end];
    let presize = body.find("presize_loader_classes(ctx").expect("presize call");
    let reserve = body.find("reserve_mirror_for_define(").expect("reservation");
    let define = body.find("ctx.define_class_full(").expect("define");
    let add = body.find("record_with_loader(ctx").expect("addClass");
    assert!(presize < reserve, "the presize must precede the reservation");
    assert!(reserve < define && define < add);
    // zt t7/x4b: the `mirror-reserve` row is closed on the presize's and
    // the reservation's error returns too, not only on success (three
    // `phase_end`s before the define; t6 had one).
    let ends = body[..define].matches("DefinePhase::MirrorReserve").count();
    assert_eq!(ends, 3, "phase_end on both error arms and on success");
}
