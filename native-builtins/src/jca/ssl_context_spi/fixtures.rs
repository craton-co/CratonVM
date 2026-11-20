use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};
use cratonvm_types::ClassId;

/// The real JDK 25 layout of `javax.net.ssl.SSLContext`:
/// `provider` (0), `contextSpi` (1), `protocol` (2). Verified with
/// `javap -p --module java.base javax.net.ssl.SSLContext`.
pub fn declare_real_ssl_context(ctx: &MockNativeContext, cid: ClassId) {
    let f = |name: &str, descriptor: &str, slot: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        cid,
        vec![
            f("provider", "Ljava/security/Provider;", 0),
            f("contextSpi", "Ljavax/net/ssl/SSLContextSpi;", 1),
            f("protocol", "Ljava/lang/String;", 2),
        ],
    );
}

/// An `SSLContextSpi` subclass named `spi_class`, allocated and returned.
pub fn make_spi(ctx: &mut MockNativeContext, spi_class: &str) -> ObjectRef {
    let base = ctx
        .ensure_class_initialized("javax/net/ssl/SSLContextSpi")
        .expect("spi base");
    let cid = ctx.ensure_class_initialized(spi_class).expect("spi class");
    ctx.set_superclass(cid, base);
    ctx.alloc_object(cid, 1)
}

/// A REAL `javax.net.ssl.SSLContext` around `spi_class`, with `protocol`
/// set to `protocol`.
pub fn make_real_context(
    ctx: &mut MockNativeContext,
    spi_class: &str,
    protocol: &str,
) -> (ObjectRef, ObjectRef) {
    let spi = make_spi(ctx, spi_class);
    let cid = ctx
        .ensure_class_initialized("javax/net/ssl/SSLContext")
        .expect("context class");
    declare_real_ssl_context(ctx, cid);
    let obj = ctx.alloc_object(cid, 3);
    let provider = ctx.create_string("a Provider stand-in");
    let proto = ctx.create_string(protocol);
    ctx.set_field(obj, 0, Value::Object(Some(provider)));
    ctx.set_field(obj, 1, Value::Object(Some(spi)));
    ctx.set_field(obj, 2, Value::Object(Some(proto)));
    (obj, spi)
}

/// The shape `net_phase_e::register_re6_ssl_context::getInstance` builds:
/// the real class, slot 0 = protocol String, slot 1 = an `Int` init flag.
pub fn make_synthetic_context(ctx: &mut MockNativeContext, protocol: &str) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("javax/net/ssl/SSLContext")
        .expect("context class");
    declare_real_ssl_context(ctx, cid);
    let obj = ctx.alloc_object(cid, 3);
    let proto = ctx.create_string(protocol);
    ctx.set_field(obj, 0, Value::Object(Some(proto)));
    ctx.set_field(obj, 1, Value::Int(0));
    obj
}
