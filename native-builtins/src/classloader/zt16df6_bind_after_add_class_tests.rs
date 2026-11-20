//! zt t16/df6 (`docs/feature-designs/zgc-architecture.md`
//! §1, corner 2 and D6): the late name binding of the `defineClass0/1/2`
//! doors (`CRATONVM_ZGC_DEFINE_BIND_AFTER_ADD_CLASS`).
use super::*;
use crate::test_utils::{mock_heap_is_zgc, MockNativeContext};
// zt t18/df7: `new_object` below is a `NativeHeapAccess` method (the t16
// suite: E0599 without it).
use cratonvm_native_api::NativeHeapAccess;

fn body<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let from = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let to = from + src[from..].find(end).unwrap_or_else(|| panic!("{end}"));
    &src[from..to]
}

/// `public class <name> extends java/lang/Object`, version 52, no members
/// (the `zt9df` fixture's shape).
fn class_bytes(name: &str) -> Vec<u8> {
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


fn indexed(table: &DefiningLoaderTable, addr: usize, cid: u32) -> usize {
    table
        .by_loader
        .get(&addr)
        .map_or(0, |entry| entry.ids.iter().filter(|&&id| id == cid).count())
}

/// An unbound define's defining-loader row is written (the mirror's
/// `mirror_pin`, `getClassLoader`, the unload hints need it) but the class
/// stays out of the loader's by-name index until it is bound; indexing it
/// twice leaves one copy; re-registering an indexed id unbound takes it
/// out again; the plain registration is unchanged.
#[test]
fn zt16df6_an_unbound_define_is_out_of_the_loader_index_until_indexed() {
    let mut table = DefiningLoaderTable::default();
    // SAFETY: never dereferenced -- the table keys on the address only.
    let loader = unsafe { ObjectRef::from_raw(0x2000 as *mut u8) };
    table.insert_with(7, loader, false);
    assert!(table.rows.contains_key(&7), "the row is written");
    assert_eq!(indexed(&table, 0x2000, 7), 0, "not indexed while unbound");
    table.index_id(7);
    assert_eq!(indexed(&table, 0x2000, 7), 1);
    table.index_id(7);
    assert_eq!(
        indexed(&table, 0x2000, 7),
        1,
        "indexing twice keeps one copy"
    );
    table.insert_with(7, loader, false);
    assert_eq!(
        indexed(&table, 0x2000, 7),
        0,
        "an unbound re-registration unindexes"
    );
    assert!(table.rows.contains_key(&7));
    table.index_id(99);
    assert_eq!(indexed(&table, 0x2000, 99), 0, "no row, nothing indexed");
    table.insert(8, loader);
    assert_eq!(
        indexed(&table, 0x2000, 8),
        1,
        "the plain registration indexes"
    );
}


/// Settling: not requested, nothing changes; requested on a context that
/// never defers (the trait defaults: bind `None`, withdraw `false`), the
/// upcall's result stands, nothing is withdrawn (so the door notes the
/// orphan as before) and the class is indexed either way.
#[test]
fn zt16df6_settling_on_a_context_that_never_defers_changes_nothing() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x16df_0001);
    let cid = cratonvm_types::ClassId::new(41);
    // SAFETY: never dereferenced.
    let loader = unsafe { ObjectRef::from_raw(0x3000 as *mut u8) };
    register_defining_loader_unbound(loader_tables(&ctx), cid.as_u32(), loader);
    let not_requested: Result<ObjectRef, MethodCallFailed> = Err(
        crate::util_concurrent_ext::native_heap_oom("java.lang.Object[]"),
    );
    let (out, withdrawn) = settle_unbound_define(&mut ctx, not_requested, cid, false);
    assert!(out.is_err() && !withdrawn);
    let unindexed = |ctx: &MockNativeContext| {
        loader_tables(ctx)
            .defining
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_loader
            .get(&0x3000)
            .map_or(0, |e| {
                e.ids.iter().filter(|&&id| id == cid.as_u32()).count()
            })
    };
    assert_eq!(
        unindexed(&ctx),
        0,
        "not requested: the door's registration stands"
    );
    let failed: Result<ObjectRef, MethodCallFailed> = Err(
        crate::util_concurrent_ext::native_heap_oom("java.lang.Object[]"),
    );
    let (out, withdrawn) = settle_unbound_define(&mut ctx, failed, cid, true);
    assert!(out.is_err(), "the upcall's failure is the define's");
    assert!(!withdrawn, "a context that never defers withdraws nothing");
    assert_eq!(unindexed(&ctx), 1, "a class that keeps its name is indexed");
    let mirror = match ctx.new_object("example/Zt16Df6Mirror").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected a mirror stand-in, got {other:?}"),
    };
    let (out, withdrawn) = settle_unbound_define(&mut ctx, Ok(mirror), cid, true);
    assert!(!withdrawn);
    assert!(out.is_ok(), "a bound (or never unbound) class is answered");
    assert_eq!(unindexed(&ctx), 1, "indexed once");
}

/// The three `defineClass0/1/2` doors: the wait before the define, the
/// request in the options, the unindexed registration, and the settle
/// between the upcall and the orphan note (which a withdrawn class skips).
/// `Unsafe.defineClass` and the synthetic-JDK doors are not converted.
#[test]
fn zt16df6_the_define_doors_settle_between_the_upcall_and_the_note() {
    let lang_system = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs")).replace("\r\n", "\n");
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
        let src = body(&lang_system, start, end);
        let gate = src
            .find("define_bind_after_add_class_enabled(ctx, loader_id)")
            .expect("gate");
        let wait = src.find("wait_for_unbound_define(").expect("wait");
        let presize = src.find("presize_for_add_class(").expect("presize");
        let define = src.find("ctx.define_class_full(").expect("define");
        let register = src
            .find("register_defining_loader_unbound(")
            .expect("unbound row");
        let add = src.find("add_defined_class_to_loader(").expect("upcall");
        let settle = src.find("settle_unbound_define(").expect("settle");
        let skip = src
            .find("if !withdrawn {")
            .expect("note skipped when withdrawn");
        let note = src.find("note_add_class_orphan_on_failure(").expect("note");
        assert!(
            gate < wait && wait < presize && presize < define,
            "{start}: wait first"
        );
        assert!(
            define < register && register < add,
            "{start}: unindexed row"
        );
        assert!(
            add < settle && settle < skip && skip < note,
            "{start}: settle, then note"
        );
        assert!(
            src.contains("bind_name_after_add_class"),
            "{start}: the request"
        );
    }
    let unsafe_natives = include_str!("../unsafe_natives.rs");
    assert!(
        !unsafe_natives.contains("bind_name_after_add_class"),
        "Unsafe.defineClass binds at the define (page §1: not converted)"
    );
    // The duplicate classification sees this loader's own unbound define.
    let classify = body(&lang_system, "fn classify_duplicate_define(", "\n}\n");
    let unbound = classify
        .find("ctx.unbound_define_of(internal_name, loader_id)")
        .expect("unbound");
    let object = classify
        .find("class_defined_by_this_loader_object(ctx, loader_obj, internal_name)")
        .expect("object question");
    assert!(unbound < object, "the unbound define is asked first");
}
