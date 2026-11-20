/// gci i15/rb (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
/// findings 1 and 4): `ForceGarbageCollection` runs its collection (and
/// the forced finalizer / cleaner drains behind it) through
/// `on_calling_thread`, i.e. out of native under the JNI entry, and builds
/// no context of its own; it is the VM's own full collection
/// (`force_gc_for_vm`: no `-XX:+DisableExplicitGC`, no `jdk.SystemGC`, as
/// HotSpot's `_jvmti_force_gc`); the tag sweep (which posts `ObjectFree`)
/// comes after. (Source pin: a JNI in-native caller cannot be staged in a
/// unit test of this module.)
#[test]
fn gci_i15rb_force_garbage_collection_leaves_native_first() {
    let src = include_str!("../heap.rs").replace("\r\n", "\n");
    let start = src
        .find("pub(super) extern \"C\" fn force_garbage_collection(")
        .expect("force_garbage_collection");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n}\n").expect("its end")];
    let gc = body
        .find("on_calling_thread(&shared, |ctx| ctx.force_gc_for_jvmti())")
        .expect("the collection runs under the calling-thread entry");
    let sweep = body.find("sweep_all_tags(&shared)").expect("the tag sweep");
    assert!(gc < sweep, "the sweep follows the collection");
    assert!(
        !body.contains("NativeContextImpl"),
        "no context outside the entry"
    );
    assert!(
        !body.contains("current_jvm_thread_of"),
        "no raw thread borrow"
    );
}
