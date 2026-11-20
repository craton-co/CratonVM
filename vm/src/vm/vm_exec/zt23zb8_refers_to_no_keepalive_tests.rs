//! zt t23/zb8: `refersTo` reads the referent without keeping it alive
//! (`NativeHeapAccess::get_field_no_keepalive`, ZGC's `Weak` load while
//! the read barrier is armed). The GC half (the weak read never marks,
//! the strong one does) is st1's `zt23st1_get_field_weak_never_marks_under_the_flip`;
//! these pin the wiring.

fn body_of<'a>(src: &'a str, head: &str, end: &str) -> &'a str {
    let start = src.find(head).expect("the function");
    let len = src[start..].find(end).expect("its end");
    &src[start..start + len]
}

/// The override: unarmed it is exactly `get_field`; armed, the forwarded
/// receiver's slot goes through `get_field_weak`.
#[test]
fn zt23zb8_the_override_reads_weak_only_while_the_barrier_is_armed() {
    let src = include_str!("../vm_exec/new_object.rs").replace("\r\n", "\n");
    let body = body_of(
        &src,
        concat!("    fn get_field_no_", "keepalive(&self"),
        "\n    \u{7d}\n",
    );
    let armed = body
        .find("zgc_load_barrier_armed()")
        .expect("the armed gate");
    let strong = body
        .find("return self.get_field(obj, index);")
        .expect("fallback");
    let weak = body
        .find(".get_field_weak(obj, index)")
        .expect("the weak read");
    assert!(body.contains("if !self.shared.mem.heap.zgc_load_barrier_armed()"));
    assert!(armed < strong && strong < weak);
    assert!(body.contains("load_and_forward_checked(obj)"));
    assert!(!body.contains("referent_keep_alive"));
    assert!(!body.contains("satb_barrier"));
}

/// Every `refersTo0` reader of the referent: the two natives, the JIT
/// direct helper and the leaf arm.
#[test]
fn zt23zb8_every_refers_to_reader_takes_the_no_keepalive_read() {
    let helpers = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let direct = body_of(
        &helpers,
        concat!("unsafe fn jit_refers_to0_", "body("),
        "\n\u{7d}\n",
    );
    assert!(direct.contains("ctx.get_field_no_keepalive(this, 0)"));
    assert!(!direct.contains("ctx.get_field(this, 0)"));
    let leaf = body_of(
        &helpers,
        concat!("LeafNativeKind::Reference", "RefersTo => \u{7b}"),
        "LeafNativeKind::ClassIsInstance => \u{7b}",
    );
    assert!(leaf.contains("ctx.get_field_no_keepalive("));
    assert!(!leaf.contains("ctx.get_field("));

    let builtins =
        include_str!("../../../../native-builtins/src/lib_parts/native_printwriter_flush.rs")
            .replace("\r\n", "\n");
    let native = body_of(
        &builtins,
        concat!("fn native_reference_refers_", "to(ctx"),
        "\n\u{7d}\n",
    );
    assert!(native.contains("ctx.get_field_no_keepalive(this, 0)"));
    assert!(!native.contains("ctx.get_field(this"));
    let reference =
        include_str!("../../../../native-builtins/src/reference.rs").replace("\r\n", "\n");
    let native = body_of(
        &reference,
        concat!("fn native_ref_refers_", "to(ctx"),
        "\n\u{7d}\n",
    );
    assert!(native.contains("ctx.get_field_no_keepalive(this, REF_FIELD_REFERENT)"));
    assert!(!native.contains("ctx.get_field(this"));
}
