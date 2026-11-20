#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};

/// Build a `java.lang.Module` whose field 0 (the name) is either a String
/// or null. Null is how CratonVM represents both the unnamed module and a
/// module it simply cannot name.
fn module_obj(ctx: &mut MockNativeContext, name: Option<&str>) -> Value {
    let cid = ctx.ensure_class_initialized("java/lang/Module").unwrap();
    let m = ctx.alloc_object(cid, 4);
    match name {
        Some(n) => {
            let s = ctx.create_string(n);
            ctx.set_field(m, 0, Value::Object(Some(s)));
        }
        None => ctx.set_field(m, 0, Value::Object(None)),
    }
    Value::Object(Some(m))
}

#[test]
fn no_target_argument_stays_unqualified() {
    let mut ctx = mock_ctx();
    // `Module.implAddOpens(String)` and the `--add-opens` CLI path have no
    // target module at all; those really are unqualified.
    assert_eq!(dynamic_edge_target(&mut ctx, &[], None), "");
}

#[test]
fn named_target_module_is_recorded_verbatim() {
    let mut ctx = mock_ctx();
    let target = module_obj(&mut ctx, Some("jdk.compiler"));
    let args = [Value::Object(None), Value::Object(None), target];
    assert_eq!(
        dynamic_edge_target(&mut ctx, &args, Some(2)),
        "jdk.compiler"
    );
}

/// The regression guard. A target module CratonVM cannot name must NOT
/// collapse to the empty string: `ModuleRegistry::add_opens` reads an empty
/// target as *unqualified*, i.e. opened to every module in the process.
///
/// Mockito's `InstrumentationMemberAccessor` opens `java.base/java.lang` to
/// its own ByteBuddy-injected module; widening that to "everyone" made
/// `Module.isOpen("java.lang")` answer true where HotSpot answers false and
/// broke `AotIntegrationTests#endToEndTestsForBeanOverrides`.
#[test]
fn unnameable_target_module_is_not_recorded_as_unqualified() {
    let mut ctx = mock_ctx();
    let target = module_obj(&mut ctx, None);
    let args = [Value::Object(None), Value::Object(None), target];
    let resolved = dynamic_edge_target(&mut ctx, &args, Some(2));
    assert!(
        !resolved.is_empty(),
        "an empty target means unqualified/open-to-everyone"
    );
    // A `Module` whose `name` field is null is an unnamed module, recorded
    // by its loader (null here: the bootstrap loader's) in both modes since
    // interpreter round i1 wave 47 -- a qualified target, never "everyone".
    assert_eq!(
        resolved,
        cratonvm_classloading::module::unnamed_module_of_loader_target(
            cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP,
        )
    );
}
