/// The text of `name`'s item, from its line up to the next `}` at column 0.
fn item<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(name).expect(name);
    let rest = &src[start..];
    &rest[..rest.find("\n}\n").expect("its end")]
}

/// gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
/// items 1 and 2): `GetTag` / `SetTag` decode and use the object under one
/// JNI entry; `GetObjectsWithTags` takes the entry before it resolves the
/// weak globals and mints the handles under it; every `ObjectFree` is
/// posted after the entry is dropped. (Source pin: a collection cannot be
/// staged between the decode and the lookup in a unit test.)
#[test]
fn gci_i9hj_tag_functions_hold_the_entry_across_the_lookup() {
    let src = include_str!("../heap.rs").replace("\r\n", "\n");
    let wot = item(&src, "fn with_object_tags<");
    // gci i48 lane jl48: the vm-only entry (the tag functions run no Java).
    let enter = wot
        .find("jni::ForeignJniEntry::enter_vm_only()")
        .expect("entry");
    let decode = wot.find("jni::jobject_to_obj(handle)").expect("decode");
    let table = wot.find("refreshed_tags(env, shared,").expect("table op");
    let closed = wot.find("\n    };\n").expect("entry scope ends");
    let posted = wot.find("post_freed_tags(env, &freed)").expect("posted");
    assert!(enter < decode && decode < table && table < closed && closed < posted);
    for f in [
        "pub(super) extern \"C\" fn get_tag(",
        "pub(super) extern \"C\" fn set_tag(",
    ] {
        let b = item(&src, f);
        assert!(b.contains("with_object_tags(e, &shared, object,"), "{f}");
        assert!(!b.contains("object_of("), "{f}");
        assert!(!b.contains("with_tags("), "{f}");
    }
    let gowt = item(&src, "pub(super) extern \"C\" fn get_objects_with_tags(");
    let enter = gowt.find("jni::ForeignJniEntry::enter()").expect("entry");
    let resolve = gowt.find("refreshed_tags(e, &shared,").expect("resolve");
    let mint = gowt.find("jni::new_local_handle(obj)").expect("mint");
    let posted = gowt.find("post_freed_tags(e, &freed)").expect("posted");
    assert!(enter < resolve && resolve < mint && mint < posted);
    assert!(
        !gowt.contains("with_tags(e,"),
        "no resolve before the entry"
    );
    // `with_tags` itself never takes an entry: it may run in native.
    assert!(!item(&src, "fn refreshed_tags<").contains("ForeignJniEntry"));
}
