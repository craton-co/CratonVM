use super::*;
use crate::test_utils::{mock_heap_is_zgc, mock_method_exists, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

thread_local! {
    /// `(receiver class id, argument address)` of each `addClass` call.
    static ADD_CLASS_CALLS: std::cell::RefCell<Vec<(u32, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `ArrayList.add` calls (the presize's growth).
    static LIST_ADDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn calls() -> Vec<(u32, usize)> {
    ADD_CLASS_CALLS.with(|c| c.borrow().clone())
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

/// A loader on the REAL `ClassLoader` layout (a witness field declared),
/// its `classes` list holding `size` of `capacity` slots (`None`: the
/// field is null), and a stand-in mirror. `addClass` exists.
fn fixture(
    ctx: &mut MockNativeContext,
    list: Option<(usize, usize)>,
) -> (ObjectRef, ObjectRef) {
    declare(ctx, CL_CLASS, &["classes", "nameAndId"]);
    declare(ctx, "java/util/ArrayList", &["elementData", "size"]);
    let loader = object(ctx, CL_CLASS);
    if let Some((size, capacity)) = list {
        let classes = object(ctx, "java/util/ArrayList");
        let data = ctx.new_ref_array(cratonvm_types::ClassId::new(1), capacity);
        for i in 0..size {
            let element = object(ctx, "example/Zt7ClElement");
            ctx.set_array_element(data, i, Value::Object(Some(element)));
        }
        ctx.set_field_by_name(classes, "elementData", Value::Object(Some(data)));
        ctx.set_field_by_name(classes, "size", Value::Int(size as i32));
        ctx.set_field_by_name(loader, "classes", Value::Object(Some(classes)));
    }
    let mirror = object(ctx, "example/Zt7ClMirror");
    mock_method_exists(CL_CLASS, "addClass", ADD_CLASS_DESCRIPTOR);
    ADD_CLASS_CALLS.with(|c| c.borrow_mut().clear());
    LIST_ADDS.with(|c| c.set(0));
    (loader, mirror)
}

/// `addClass` records its call; `ArrayList.add` grows the list (10, then
/// 1.5x) and stores.
fn hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match (method_name, descriptor) {
        ("addClass", ADD_CLASS_DESCRIPTOR) => {
            let arg = match args.first() {
                Some(Value::Object(Some(m))) => m.as_ptr() as usize,
                _ => 0,
            };
            let cid = ctx.class_id_of_object(receiver).as_u32();
            ADD_CLASS_CALLS.with(|c| c.borrow_mut().push((cid, arg)));
            Some(Ok(None))
        }
        ("add", "(Ljava/lang/Object;)Z") => {
            LIST_ADDS.with(|c| c.set(c.get() + 1));
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
                let grown = ctx.new_ref_array(
                    cratonvm_types::ClassId::new(1),
                    (capacity + (capacity >> 1)).max(10),
                );
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
        _ => None,
    }
}

/// `addClass` on a spent heap: the growth throws.
fn refusing_hook(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "addClass" {
        return None;
    }
    Some(Err(crate::util_concurrent_ext::native_heap_oom(
        "java.lang.Object[]",
    )))
}

thread_local! {
    /// zt t8/x5: the class each EXACT (`invoke_special`) `addClass` call
    /// named; empty while every call went through the virtual door.
    static SPECIAL_CLASSES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// zt t8/x5: the exact upcall (`invoke_special`, receiver in `args[0]`)
/// served by [`hook`], with the named class recorded.
fn special_hook(
    ctx: &mut MockNativeContext,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(receiver))) = args.first().copied() else {
        return None;
    };
    SPECIAL_CLASSES.with(|c| c.borrow_mut().push(class_name.to_string()));
    hook(ctx, receiver, method_name, descriptor, &args[1..])
}

/// zt t8/x5: [`refusing_hook`] behind the exact upcall.
fn refusing_special_hook(
    ctx: &mut MockNativeContext,
    _class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(receiver))) = args.first().copied() else {
        return None;
    };
    refusing_hook(ctx, receiver, method_name, descriptor, &args[1..])    }


#[test]
fn zt7cl_the_defined_class_is_added_to_its_loaders_list() {
    let mut ctx = MockNativeContext::new();
    let (loader, mirror) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_virtual_hook(hook);
    ctx.set_invoke_hook(special_hook);
    let loader_cid = ctx.class_id_of_object(loader).as_u32();

    let out = add_defined_class_to_loader(&mut ctx, loader, mirror).expect("addClass");

    assert_eq!(out, mirror, "a non-moving heap: the same mirror back");
    assert_eq!(
        calls(),
        vec![(loader_cid, mirror.as_ptr() as usize)],
        "one addClass, on the loader, with the mirror"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "the mirror pin is released");
}

/// The upcall is a GC point: the mirror handed back is the one the call
/// saw, re-read through its pin (the input address is retired).
#[test]
fn zt7cl_the_mirror_is_reread_across_the_upcall() {
    let mut ctx = MockNativeContext::new();
    let (loader, mirror) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_virtual_hook(hook);
    ctx.set_invoke_hook(special_hook);
    ctx.set_moving_heap_vm_rule(true);

    let out = add_defined_class_to_loader(&mut ctx, loader, mirror).expect("addClass");

    let seen = calls();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        out.as_ptr() as usize,
        seen[0].1,
        "the returned mirror is the forwarded one the upcall stored"
    );
    let _ = ctx.class_id_of_object(out); // panics on a retired address
}


/// A loader whose `classes` is null (a VM-made loader object) is left
/// alone: `addClass` would throw `NullPointerException` there.
#[test]
fn zt7cl_a_loader_without_a_list_makes_no_upcall() {
    let mut ctx = MockNativeContext::new();
    let (loader, mirror) = fixture(&mut ctx, None);
    ctx.set_invoke_virtual_hook(hook);
    ctx.set_invoke_hook(special_hook);

    add_defined_class_to_loader(&mut ctx, loader, mirror).expect("no call");

    assert!(calls().is_empty());
}

/// The upcall's `OutOfMemoryError` is the define's (HotSpot propagates
/// `addClass`'s exception), with the pin released.
#[test]
fn zt7cl_a_refused_upcall_is_the_defines_error() {
    let mut ctx = MockNativeContext::new();
    let (loader, mirror) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_virtual_hook(refusing_hook);
    ctx.set_invoke_hook(refusing_special_hook);

    let err = add_defined_class_to_loader(&mut ctx, loader, mirror).expect_err("refused");

    assert!(format!("{err:?}").contains("OutOfMemoryError"), "{err:?}");
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}


/// On the real layout `classes` is read through `ClassLoader`'s declared
/// slot, not by name.
#[test]
fn zt7cl_the_list_is_read_through_the_declared_slot() {
    let mut ctx = MockNativeContext::new();
    let (loader, _) = fixture(&mut ctx, Some((0, 0)));
    assert!(!cl_has_synthetic_layout(&ctx, loader), "the fixture is the real layout");
    let index = ctx.resolve_field_index(CL_CLASS, "classes").expect("declared");
    assert_eq!(
        loader_classes_list(&ctx, loader).map(|l| Value::Object(Some(l))),
        Some(ctx.get_field(loader, index))
    );
}

/// The order every real-JDK door keeps: the presize before the mirror
/// reservation, the define, then the upcall after it.
#[test]
fn zt7cl_every_door_presizes_before_the_reservation_and_adds_after_the_define() {
    fn body<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
        let from = src.find(start).unwrap_or_else(|| panic!("{start}"));
        let to = from + src[from..].find(end).unwrap_or_else(|| panic!("{end}"));
        &src[from..to]
    }
    let lang_system = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs"));
    let unsafe_natives = include_str!("../unsafe_natives.rs");
    let lookup_define = include_str!("../lookup_define.rs");
    let doors = [
        (
            body(
                lang_system,
                "pub fn native_classloader_define_class1(",
                "pub fn native_classloader_define_class2(",
            ),
            "reserve_defined_class_mirror(ctx",
        ),
        (
            body(
                lang_system,
                "pub fn native_classloader_define_class2(",
                "pub fn native_classloader_define_class0(",
            ),
            "reserve_defined_class_mirror(ctx",
        ),
        (
            body(
                lang_system,
                "pub fn native_classloader_define_class0(",
                "pub fn native_perf_attach(",
            ),
            "reserve_mirror_for_define(ctx",
        ),
        (
            body(
                unsafe_natives,
                "pub fn native_unsafe_define_class(",
                "fn native_unsafe_define_anonymous_class(",
            ),
            "reserve_mirror_for_define(",
        ),
        (
            body(lookup_define, "fn lk_define_class_b_pinned(", "\n}"),
            "reserve_mirror_for_define(ctx",
        ),
    ];
    for (i, (src, reserve)) in doors.iter().enumerate() {
        let presize = src.find("presize_for_add_class(ctx").expect("presize");
        let reserve = src.find(reserve).expect("reservation");
        let define = src.find("ctx.define_class_full(").expect("define");
        // zt t9/df: the three `lang_system` doors break the call's
        // arguments onto the next line (the orphan note follows it).
        let add = src.find("add_defined_class_to_loader(").expect("upcall");
        assert!(presize < reserve, "door {i}: the presize precedes the reservation");
        assert!(reserve < define && define < add, "door {i}: the upcall follows the define");
    }
}

// ---- zt t9/df: the define path's residuals ----------------------------

/// zt t9/df: `public class <name> extends java/lang/Object {}` (version
/// 52), the class file the probes define.
fn zt9df_class_bytes(name: &str) -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&5u16.to_be_bytes());
    for (index, text) in [(2u16, name), (4u16, "java/lang/Object")] {
        b.push(7);
        b.extend_from_slice(&index.to_be_bytes());
        b.push(1);
        b.extend_from_slice(&(text.len() as u16).to_be_bytes());
        b.extend_from_slice(text.as_bytes());
    }
    b.extend_from_slice(&0x0021u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    b
}

/// zt t9/df: a heap refusal, as a native door raises it.
fn zt9df_oom<T>() -> Result<T, MethodCallFailed> {
    Err(crate::util_concurrent_ext::native_heap_oom("java.lang.Object[]"))
}

/// zt t9/df: `ArrayList.add` refused (the presize's growth on a spent
/// heap); `addClass` is served by [`hook`].
fn zt9df_add_refused_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "add" {
        return Some(zt9df_oom());
    }
    hook(ctx, receiver, method_name, descriptor, args)
}


/// zt t9/df (x5 item 1, zt7cl item 1): a failed upcall leaves an orphan;
/// a retry of OTHER bytes is not adopted (the caller's `LinkageError`
/// stands), a retry of the SAME bytes makes the upcall and answers the
/// class, and the row is claimed, so a second retry is the duplicate
/// definition again.
#[test]
fn zt9df_a_retry_of_the_same_bytes_adopts_the_orphan_of_a_failed_upcall() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x29df_0001);
    let (loader, _) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_virtual_hook(hook);
    ctx.set_invoke_hook(special_hook);
    mock_heap_is_zgc(true);
    let cid = ctx.ensure_class_initialized("zt9df/Orphan").expect("class");
    register_defining_loader(loader_tables(&ctx), cid.as_u32(), loader);
    let bytes = zt9df_class_bytes("zt9df/Orphan");
    let other = zt9df_class_bytes("zt9df/Other");

    let served: Result<ObjectRef, MethodCallFailed> = Ok(loader);
    note_add_class_orphan_on_failure(&ctx, &served, cid, &bytes);
    assert!(
        adopt_add_class_orphan(&mut ctx, loader, "zt9df/Orphan", 7, &bytes, None).is_none(),
        "a served upcall leaves no orphan"
    );
    note_add_class_orphan_on_failure(&ctx, &zt9df_oom(), cid, &bytes);
    assert!(
        adopt_add_class_orphan(&mut ctx, loader, "zt9df/Orphan", 7, &other, None).is_none(),
        "other bytes: the duplicate definition stands"
    );
    assert!(calls().is_empty(), "no upcall for other bytes");

    let adopted = adopt_add_class_orphan(&mut ctx, loader, "zt9df/Orphan", 7, &bytes, None)
        .expect("the orphan is adopted")
        .expect("the upcall is served");

    assert_eq!(ctx.class_id_from_mirror(adopted), Some(cid), "the class it left");
    let loader_cid = ctx.class_id_of_object(loader).as_u32();
    assert_eq!(calls().len(), 1, "one addClass");
    assert_eq!(calls()[0].0, loader_cid, "on the defining loader");
    assert!(
        adopt_add_class_orphan(&mut ctx, loader, "zt9df/Orphan", 7, &bytes, None).is_none(),
        "claimed: a second retry is the duplicate definition"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "the loader pin is released");
    mock_heap_is_zgc(false);
}


/// zt t9/df: an adoption whose upcall fails again keeps the orphan, so a
/// later retry (with room) still adopts it.
#[test]
fn zt9df_an_adoption_whose_upcall_fails_keeps_the_orphan() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x29df_0002);
    let (loader, _) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_virtual_hook(refusing_hook);
    ctx.set_invoke_hook(refusing_special_hook);
    mock_heap_is_zgc(true);
    let cid = ctx.ensure_class_initialized("zt9df/Again").expect("class");
    register_defining_loader(loader_tables(&ctx), cid.as_u32(), loader);
    let bytes = zt9df_class_bytes("zt9df/Again");
    note_add_class_orphan_on_failure(&ctx, &zt9df_oom(), cid, &bytes);

    let again = adopt_add_class_orphan(&mut ctx, loader, "zt9df/Again", 7, &bytes, None)
        .expect("the orphan is found");
    assert!(again.is_err(), "the upcall's refusal is the retry's error");

    ctx.set_invoke_virtual_hook(hook);
    ctx.set_invoke_hook(special_hook);
    let adopted = adopt_add_class_orphan(&mut ctx, loader, "zt9df/Again", 7, &bytes, None)
        .expect("still an orphan");
    assert!(adopted.is_ok(), "the third attempt is served");
    assert_eq!(ctx.native_pin_count_for_test(), 0);
    mock_heap_is_zgc(false);
}



/// zt t9/df: the presize's refused growth (a full list on a spent heap)
/// is the define's `OutOfMemoryError` for bytes that parse, and is
/// skipped for bytes that do not (the monitor is released either way).
#[test]
fn zt9df_a_refused_presize_yields_to_malformed_bytes() {
    let mut ctx = MockNativeContext::new();
    let (loader, _) = fixture(&mut ctx, Some((10, 10)));
    ctx.set_invoke_virtual_hook(zt9df_add_refused_hook);
    mock_heap_is_zgc(true);
    let good = zt9df_class_bytes("zt9df/Presized");
    let bad = good[..12].to_vec();

    let err = presize_for_add_class(&mut ctx, Some(loader), &good, "").expect_err("refused");
    assert!(format!("{err:?}").contains("OutOfMemoryError"), "{err:?}");
    presize_for_add_class(&mut ctx, Some(loader), &bad, "").expect("malformed: skipped");

    mock_heap_is_zgc(false);
    assert_eq!(ctx.monitor_depth.get(), 0, "the monitor is released");
    let classes = loader_classes_list(&ctx, loader).expect("list");
    assert_eq!(ctx.get_field_by_name(classes, "size"), Value::Int(10), "unchanged");
}


/// zt t9/df: in `defineClass0` the upcall precedes the deferred
/// initialization, and the define itself initializes only when nothing is
/// deferred; the three `lang_system` doors note an orphan after the upcall
/// and offer it to the duplicate-definition arm.
#[test]
fn zt9df_define_class0_initializes_after_the_upcall_and_the_doors_adopt_orphans() {
    fn body<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
        let from = src.find(start).unwrap_or_else(|| panic!("{start}"));
        let to = from + src[from..].find(end).unwrap_or_else(|| panic!("{end}"));
        &src[from..to]
    }
    let lang_system = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs"));
    let define0 = body(
        lang_system,
        "pub fn native_classloader_define_class0(",
        "pub fn native_perf_attach(",
    );
    assert!(define0.contains("initialize: initialize && !init_after_define,"));
    let add = define0.find("add_defined_class_to_loader(").expect("upcall");
    let init = define0.find("if init_after_define {").expect("deferred init");
    assert!(add < init, "the upcall precedes the deferred <clinit>");
    for (start, end) in [
        (
            "pub fn native_classloader_define_class1(",
            "pub fn native_classloader_define_class2(",
        ),
        (
            "pub fn native_classloader_define_class2(",
            "pub fn native_classloader_define_class0(",
        ),
        (
            "pub fn native_classloader_define_class0(",
            "pub fn native_perf_attach(",
        ),
    ] {
        let src = body(lang_system, start, end);
        let add = src.find("add_defined_class_to_loader(").expect("upcall");
        let note = src.find("note_add_class_orphan_on_failure(").expect("note");
        let same = src.find("DuplicateDefine::SameLoaderObject =>").expect("dup arm");
        let adopt = src.find("adopt_add_class_orphan(").expect("adoption");
        assert!(add < note, "{start}: the note follows the upcall");
        assert!(same < adopt, "{start}: the adoption is in the duplicate arm");
        let refusal = src.find("refusal_yields_to_format_error(").expect("format first");
        let define = src.find("ctx.define_class_full(").expect("define");
        assert!(refusal < define, "{start}: the reservation's rescue precedes the define");
    }
}

/// zt t9/df: the built-in loader objects behind a lookup class's tag
/// (`NATIVE_EXTENSION` 1, `NATIVE_APPLICATION` 2); none for the boot tag
/// or a user namespace.
#[test]
fn zt9df_builtin_loader_for_add_class_answers_the_singletons() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x29df_0005);
    let (app, _) = fixture(&mut ctx, Some((0, 0)));
    let (platform, _) = fixture(&mut ctx, Some((0, 0)));
    set_app_loader(loader_tables(&ctx), Some(app));
    set_platform_loader(loader_tables(&ctx), Some(platform));

    assert_eq!(builtin_loader_for_add_class(&ctx, 2), Some(app));
    assert_eq!(builtin_loader_for_add_class(&ctx, 1), Some(platform));
    assert_eq!(builtin_loader_for_add_class(&ctx, 0), None);
    assert_eq!(builtin_loader_for_add_class(&ctx, 3), None);
    set_app_loader(loader_tables(&ctx), None);
    set_platform_loader(loader_tables(&ctx), None);
}


// ---- zt t10/df2: the define path, continued ---------------------------

/// zt t10/df2 (`zgc-zt9df-define-path-residuals-SUPERSEDED-20261019.md` §1, the proxy
/// cache): an orphan noted without bytes (the `Proxy` bridges' generated
/// class) is pending, adopted by id with one `addClass` on the loader, and
/// claimed, so the next cache hit makes no upcall.
#[test]
fn zt10df2_an_orphan_is_adopted_by_id_once() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x2a0d_f201);
    let (loader, _) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_virtual_hook(hook);
    ctx.set_invoke_hook(special_hook);
    mock_heap_is_zgc(true);
    let cid = ctx.ensure_class_initialized("zt10df2/Proxy0").expect("class");
    assert!(!add_class_orphan_pending(&ctx, cid), "no orphan yet");
    assert!(adopt_add_class_orphan_by_id(&mut ctx, loader, cid).is_none());
    assert!(calls().is_empty(), "not an orphan: no upcall");

    note_add_class_orphan_on_failure(&ctx, &zt9df_oom(), cid, &[]);
    assert!(add_class_orphan_pending(&ctx, cid), "the refused upcall is noted");
    let adopted = adopt_add_class_orphan_by_id(&mut ctx, loader, cid)
        .expect("the orphan is adopted")
        .expect("the upcall is served");

    assert_eq!(ctx.class_id_from_mirror(adopted), Some(cid), "the class it left");
    assert_eq!(calls().len(), 1, "one addClass");
    assert_eq!(calls()[0].0, ctx.class_id_of_object(loader).as_u32(), "on the loader");
    assert!(!add_class_orphan_pending(&ctx, cid), "claimed");
    assert!(adopt_add_class_orphan_by_id(&mut ctx, loader, cid).is_none());
    assert_eq!(calls().len(), 1, "a second cache hit makes no upcall");
    assert_eq!(ctx.native_pin_count_for_test(), 0, "the loader pin is released");
    mock_heap_is_zgc(false);
}




/// zt t12/df4 (define page §8, D8): a `Proxy` bridge's fresh upcall is
/// marked in flight before the cache publish and the mark is gone when
/// the upcall ends: a served upcall leaves nothing, a refused one leaves
/// the orphan a waiting racer adopts. The mark reads as "not defined yet"
/// to `findLoadedClass`. The bridges mark through
/// `proxy_class_for_upcall`, before the cache insert.
thread_local! {
    /// zt df4 D8: `(class id, Thread.yield calls)` of
    /// [`d8_yield_hook`]'s race.
    static D8_RACE: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// zt df4 D8: the racer's `Thread.yield()` rounds; the third ends the
/// other thread's upcall (its mark is cleared). Everything else is
/// [`special_hook`].
fn d8_yield_hook(
    ctx: &mut MockNativeContext,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if class_name == "java/lang/Thread" && method_name == "yield" {
        let (cid, n) = D8_RACE.with(std::cell::Cell::get);
        D8_RACE.with(|c| c.set((cid, n + 1)));
        if n + 1 == 3 {
            clear_add_class_upcall_in_flight(&*ctx, cratonvm_types::ClassId::new(cid));
        }
        return Some(Ok(None));
    }
    special_hook(ctx, class_name, method_name, descriptor, args)
}

/// zt df4 D8 (`CRATONVM_ZGC_PROXY_UPCALL_SERIALIZED`, default ON): a
/// cache hit on a `$ProxyN` whose fresh define's `addClass` upcall another
/// thread still has in flight waits (GC-safe `Thread.yield()` rounds) until
/// that upcall ends, as HotSpot's `ProxyBuilder` memoizer makes a racer
/// wait, and then answers without an upcall of its own (the class is in
/// its loader's list: no orphan is pending).
#[test]
fn zt31d8_a_cache_hit_waits_for_the_racers_upcall() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x2c0d_f831);
    let (loader, _) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_virtual_hook(hook);
    ctx.set_invoke_hook(d8_yield_hook);
    mock_heap_is_zgc(true);
    {
        assert!(crate::reflect_annotations::proxy_upcall_serialized(&ctx), "default ON");
    }
    let cid = ctx.ensure_class_initialized("zt31d8/Proxy0").expect("class");
    // Another thread's fresh define marked it (not this thread's mark).
    mark_add_class_upcall_in_flight(&ctx, cid);
    D8_RACE.with(|c| c.set((cid.as_u32(), 0)));
    let out = crate::reflect_annotations::proxy_add_class_after_define(
        &mut ctx,
        cid,
        Some(loader),
        &[],
        false,
    );
    assert!(out.is_ok());
    assert_eq!(D8_RACE.with(std::cell::Cell::get).1, 3, "waited for the upcall to end");
    assert!(!add_class_upcall_in_flight(&ctx, cid));
    assert!(calls().is_empty(), "the racer makes no upcall of its own");
    assert_eq!(ctx.native_pin_count_for_test(), 0);
    mock_heap_is_zgc(false);
}

#[test]
fn zt12df4_a_fresh_proxy_upcall_is_marked_in_flight_until_it_ends() {
    for refused in [false, true] {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(if refused { 0x2c0d_f403 } else { 0x2c0d_f402 });
        let (loader, _) = fixture(&mut ctx, Some((0, 0)));
        if refused {
            ctx.set_invoke_virtual_hook(refusing_hook);
            ctx.set_invoke_hook(refusing_special_hook);
        } else {
            ctx.set_invoke_virtual_hook(hook);
            ctx.set_invoke_hook(special_hook);
        }
        mock_heap_is_zgc(true);
        let name = if refused {
            "zt12df4/Proxy1"
        } else {
            "zt12df4/Proxy0"
        };
        let cid = ctx.ensure_class_initialized(name).expect("class");
        assert!(!add_class_upcall_in_flight(&ctx, cid));

        mark_add_class_upcall_in_flight(&ctx, cid);
        assert!(add_class_upcall_in_flight(&ctx, cid), "marked");
        assert!(add_class_orphan_hidden(&ctx, cid), "not defined yet");
        // zt t14/df5: the mark is not an orphan; a cache hit whose wait
        // ran out must not claim it (a second upcall of a running one).
        assert!(
            !add_class_orphan_pending(&ctx, cid),
            "the mark is no orphan"
        );
        assert!(adopt_add_class_orphan_by_id(&mut ctx, loader, cid).is_none());
        assert!(add_class_upcall_in_flight(&ctx, cid), "the mark stays");
        let out = crate::reflect_annotations::proxy_add_class_after_define(
            &mut ctx,
            cid,
            Some(loader),
            &[],
            true,
        );

        assert_eq!(out.is_err(), refused, "the upcall's own answer");
        assert!(!add_class_upcall_in_flight(&ctx, cid), "the mark is gone");
        assert_eq!(
            add_class_orphan_pending(&ctx, cid),
            refused,
            "a refused upcall leaves the orphan for a racer to adopt"
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
        mock_heap_is_zgc(false);
    }

    let reflect = include_str!("../reflect_annotations.rs");
    let from = reflect
        .find("pub fn define_or_get_proxy_class_noting(")
        .expect("define");
    let define = &reflect[from..from + reflect[from..].find("\n}").expect("end")];
    let mark = define
        .find("mark_add_class_upcall_in_flight(")
        .expect("mark");
    let publish = define.find("map.insert(cache_key, cid)").expect("publish");
    assert!(mark < publish, "marked before another thread can see it");
    // zt t14/df5: the request is taken at entry, before the define (a
    // nested proxy define inside it must not consume it).
    let taken = define
        .find("PROXY_UPCALL_MARK_REQUESTED.with(|c| c.replace(false))")
        .expect("taken");
    let defined = define.find("ctx.define_class_full(").expect("the define");
    assert!(taken < defined, "the request is this call's");
    assert_eq!(
        reflect
            .matches("proxy_class_for_upcall(ctx, loader_namespace")
            .count(),
        2,
        "both bridges mark"
    );
}

thread_local! {
    /// zt t14/x11: what [`panicking_upcall_hook`] saw of the mark.
    static OWN_MARK_SEEN: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// zt t14/x11: an `addClass` upcall that panics, after noting whether
/// the in-flight mark of the class being added is this thread's own.
fn panicking_upcall_hook(
    ctx: &mut MockNativeContext,
    _class_name: &str,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "addClass" {
        return None;
    }
    let cid = match args.get(1) {
        Some(Value::Object(Some(mirror))) => ctx.class_id_from_mirror(*mirror),
        _ => None,
    };
    OWN_MARK_SEEN.with(|s| {
        s.set(cid.map(crate::reflect_annotations::proxy_upcall_mark_held_here))
    });
    panic!("zt14x11: the upcall unwinds");
}

/// zt t14/x11 (`zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 17): the
/// D8 mark is cleared on every exit of the upcall, an unwinding one
/// included (the panic is re-raised after the clear), and while the
/// upcall runs the mark reads as this thread's own, so a nested cache
/// hit on the same thread does not wait for it. Positive control: the
/// mark is there before the call.
#[test]
fn zt14x11_the_proxy_upcall_mark_is_cleared_on_unwind_and_owned_by_its_thread() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x2c0d_1411);
    let (loader, _) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_hook(panicking_upcall_hook);
    ctx.set_invoke_virtual_hook(|ctx, receiver, name, descriptor, args| {
        let mut all = vec![Value::Object(Some(receiver))];
        all.extend_from_slice(args);
        panicking_upcall_hook(ctx, "java/lang/ClassLoader", name, descriptor, &all)
    });
    mock_heap_is_zgc(true);
    let cid = ctx
        .ensure_class_initialized("zt14x11/Proxy0")
        .expect("class");
    mark_add_class_upcall_in_flight(&ctx, cid);
    assert!(add_class_upcall_in_flight(&ctx, cid), "positive control");
    assert!(!crate::reflect_annotations::proxy_upcall_mark_held_here(cid));
    OWN_MARK_SEEN.with(|s| s.set(None));

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::reflect_annotations::proxy_add_class_after_define(
            &mut ctx,
            cid,
            Some(loader),
            &[],
            true,
        )
    }));

    assert!(unwound.is_err(), "the panic is re-raised");
    assert_eq!(
        OWN_MARK_SEEN.with(std::cell::Cell::get),
        Some(true),
        "the upcall ran, and the mark was this thread's own while it ran"
    );
    assert!(!add_class_upcall_in_flight(&ctx, cid), "cleared on unwind");
    assert!(
        !crate::reflect_annotations::proxy_upcall_mark_held_here(cid),
        "the thread's own record is gone too"
    );
    mock_heap_is_zgc(false);

    // Source pin: the request is taken on entry to the define, before
    // anything that can run Java (`define_class_full`).
    let reflect = include_str!("../reflect_annotations.rs");
    let from = reflect
        .find("pub fn define_or_get_proxy_class_noting(")
        .expect("define");
    let define = &reflect[from..from + reflect[from..].find("\n}").expect("end")];
    let take = define
        .find("let mark_requested = PROXY_UPCALL_MARK_REQUESTED")
        .expect("taken on entry");
    let java = define.find("ctx.define_class_full(").expect("the define");
    assert!(take < java);
    assert_eq!(
        define.matches("PROXY_UPCALL_MARK_REQUESTED").count(),
        1,
        "taken once, nowhere else in the define"
    );
}

/// zt t10/df2: the `Unsafe.defineClass` door notes an orphan after its
/// upcall and offers it to its duplicate arm, as `lang_system`'s doors do;
/// the `Proxy` bridges note a fresh define's refusal and adopt a cached
/// class by id; every door names itself for
/// `CRATONVM_DBG_DEFINE_ADD_CLASS_DOOR`.
#[test]
fn zt10df2_unsafe_and_proxy_doors_note_and_adopt_orphans() {
    fn body<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
        let from = src.find(start).unwrap_or_else(|| panic!("{start}"));
        let to = from + src[from..].find(end).unwrap_or_else(|| panic!("{end}"));
        &src[from..to]
    }
    let unsafe_natives = include_str!("../unsafe_natives.rs");
    let door = body(
        unsafe_natives,
        "pub fn native_unsafe_define_class(",
        "\nfn unsafe_define_app_loader_pin(",
    );
    let add = door.find("add_defined_class_to_loader(").expect("upcall");
    let note = door.find("note_add_class_orphan_on_failure(").expect("note");
    let dup = door.find("msg.contains(\"DuplicateClassDefinition\")").expect("dup");
    let adopt = door.find("adopt_add_class_orphan(").expect("adoption");
    let error = door.find("unsafe_define_duplicate_error(ctx").expect("error");
    assert!(add < note, "the note follows the upcall");
    assert!(dup < adopt && adopt < error, "the adoption precedes the LinkageError");
    assert!(door.contains("trace_add_class_door(ctx, \"unsafe\""));

    let reflect = include_str!("../reflect_annotations.rs");
    let proxy = body(reflect, "fn proxy_add_class_after_define(", "\n}");
    let by_id = proxy.find("adopt_add_class_orphan_by_id(").expect("by id");
    let add = proxy.find("add_defined_class_to_loader(").expect("upcall");
    let note = proxy.find("note_add_class_orphan_on_failure(").expect("note");
    assert!(by_id < add && add < note, "a cache hit adopts; a fresh define notes");
    assert!(proxy.contains("trace_add_class_door(ctx, \"proxy\""));

    // zt t11/df3 (x8 C6 / hand-off D2): each door names itself INSIDE its
    // own body. `lang_system.contains("\"defineClass1\"")` matched the
    // same literal elsewhere in the file (census and debug lines), so it
    // could not fail.
    let lang_system = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs"));
    for (door, start, end) in [
        (
            "\"defineClass1\"",
            "pub fn native_classloader_define_class1(",
            "pub fn native_classloader_define_class2(",
        ),
        (
            "\"defineClass2\"",
            "pub fn native_classloader_define_class2(",
            "pub fn native_classloader_define_class0(",
        ),
        (
            "\"defineClass0\"",
            "pub fn native_classloader_define_class0(",
            "pub fn native_perf_attach(",
        ),
    ] {
        let src = body(lang_system, start, end);
        let trace = src.find("trace_add_class_door(").expect("the door trace");
        let call = &src[trace..trace + src[trace..].find(");").expect("call end")];
        assert!(call.contains(door), "{start}: the trace call names {door}");
    }
}

/// zt t11/df3: a NAMELESS retry of an orphan's bytes is adopted: the class
/// is named by the bytes' `this_class`. Only a duplicate definition is
/// offered; other bytes, unreadable bytes and another refusal are not
/// adopted (the caller's error stands). Before the fix the look-up asked
/// for `""` and found nothing.
#[test]
fn zt11df3_a_nameless_retry_adopts_the_orphan_by_its_this_class() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x211d_f301);
    let (loader, _) = fixture(&mut ctx, Some((0, 0)));
    ctx.set_invoke_virtual_hook(hook);
    ctx.set_invoke_hook(special_hook);
    mock_heap_is_zgc(true);
    let cid = ctx.ensure_class_initialized("zt11df3/Nameless").expect("class");
    register_defining_loader(loader_tables(&ctx), cid.as_u32(), loader);
    let bytes = zt9df_class_bytes("zt11df3/Nameless");
    let other = zt9df_class_bytes("zt11df3/Other");
    const DUP: &str = "DuplicateClassDefinition { class_name: \"zt11df3/Nameless\" }";
    note_add_class_orphan_on_failure(&ctx, &zt9df_oom(), cid, &bytes);

    assert!(
        adopt_nameless_add_class_orphan(&mut ctx, loader, 7, &bytes, "ClassFormatError", None)
            .is_none(),
        "not a duplicate definition: nothing offered"
    );
    assert!(
        adopt_nameless_add_class_orphan(&mut ctx, loader, 7, &other, DUP, None).is_none(),
        "other bytes: the duplicate definition stands"
    );
    assert!(
        adopt_add_class_orphan(&mut ctx, loader, "", 7, &[0xCA, 0xFE], None).is_none(),
        "no readable this_class: nothing adopted"
    );
    assert!(calls().is_empty(), "no upcall so far");

    let adopted = adopt_nameless_add_class_orphan(&mut ctx, loader, 7, &bytes, DUP, None)
        .expect("the orphan is adopted by its this_class")
        .expect("the upcall is served");

    assert_eq!(ctx.class_id_from_mirror(adopted), Some(cid), "the class it left");
    assert_eq!(calls().len(), 1, "one addClass");
    assert!(
        adopt_nameless_add_class_orphan(&mut ctx, loader, 7, &bytes, DUP, None).is_none(),
        "claimed: a second retry is the duplicate definition"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "the loader pin is released");
    mock_heap_is_zgc(false);
}

/// zt t11/df3: an orphan whose class unloads before a retry adopted it
/// leaves the orphan map with it; another orphan stays.
#[test]
fn zt11df3_an_unloaded_orphan_leaves_the_orphan_map() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x211d_f302);
    let (loader, _) = fixture(&mut ctx, Some((0, 0)));
    mock_heap_is_zgc(true);
    let gone = ctx.ensure_class_initialized("zt11df3/Gone").expect("class");
    let kept = ctx.ensure_class_initialized("zt11df3/Kept").expect("class");
    register_defining_loader(loader_tables(&ctx), gone.as_u32(), loader);
    register_defining_loader(loader_tables(&ctx), kept.as_u32(), loader);
    let bytes = zt9df_class_bytes("zt11df3/Gone");
    note_add_class_orphan_on_failure(&ctx, &zt9df_oom(), gone, &bytes);
    note_add_class_orphan_on_failure(&ctx, &zt9df_oom(), kept, &bytes);

    forget_unloaded_classes(loader_tables(&ctx), &[gone.as_u32()]);

    let orphans = loader_tables(&ctx).add_class_orphans.lock();
    assert!(!orphans.contains_key(&gone.as_u32()), "the unloaded orphan's row went");
    assert!(orphans.contains_key(&kept.as_u32()), "a live orphan's row stays");
    drop(orphans);
    mock_heap_is_zgc(false);
}


/// zt t11/df3: the nameless adoption sits in `defineClass1` / `defineClass2`
/// after the name-keyed classification. (zt t12/rt: the annotation proxy's
/// opt-in upcall, `CRATONVM_ZGC_DEFINE_ADD_CLASS_ANNOTATION_PROXY`, and its
/// text pin were retired: the JDK's `AnnotationParser` serves the proxy
/// through `defineClass1`, which makes the upcall.)
#[test]
fn zt11df3_the_doors_adopt_nameless_retries() {
    fn body<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
        let from = src.find(start).unwrap_or_else(|| panic!("{start}"));
        let to = from + src[from..].find(end).unwrap_or_else(|| panic!("{end}"));
        &src[from..to]
    }
    let lang_system = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs"));
    for (start, end) in [
        (
            "pub fn native_classloader_define_class1(",
            "pub fn native_classloader_define_class2(",
        ),
        (
            "pub fn native_classloader_define_class2(",
            "pub fn native_classloader_define_class0(",
        ),
    ] {
        let src = body(lang_system, start, end);
        let classified = src.find("DuplicateDefine::NotDuplicate => {}").expect("arm");
        let nameless = src
            .find("adopt_nameless_add_class_orphan(")
            .expect("nameless adoption");
        let gate = src[classified..].find("if name.is_empty() {").expect("gate") + classified;
        assert!(classified < gate && gate < nameless, "{start}: after the classification");
    }
    // x8 C5 / hand-off D1: the namespace fallback of the adoption checks the
    // mirror's `classLoader` before the row is claimed (the mock context
    // has no namespace lookup, so this is a text pin).
    let this = concat!(include_str!("../classloader/mod.rs"), "\n", include_str!("../classloader/closed_url_classloader_ids.rs"), "\n", include_str!("../classloader/load_class_override_memo_key.rs"), "\n", include_str!("../classloader/adopt_add_class_orphan_by_id.rs"), "\n", include_str!("../classloader/builtin_classloader_find_resource_as.rs"), "\n", include_str!("../classloader/ucl_try_define_local_class.rs"));
    let adopt = body(this, "pub fn adopt_add_class_orphan(", "\n}");
    let fallback = adopt.find(".class_id_defined_by_loader_exact(").expect("fallback");
    let check = adopt
        .find("ctx.get_field_by_name(mirror, \"classLoader\")")
        .expect("loader check");
    let claim = adopt.find("orphans.remove(&cid.as_u32())").expect("claim");
    assert!(fallback < check && check < claim, "the fallback is checked before the claim");
}
