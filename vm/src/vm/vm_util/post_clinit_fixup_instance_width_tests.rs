use super::{alloc_fixup_instance, alloc_fixup_instance_at_least};
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions};
use cratonvm_types::{ClassId, ObjectRef};
use crate::config::VmConfig;
use crate::types::Value;
use crate::vm::SharedVm;

/// A version-52 class `name extends java/lang/Object` with the given
/// instance and `public static` fields, `(name, descriptor)`, and no
/// methods.
fn class_with_fields(name: &str, instance: &[(&str, &str)], statics: &[(&str, &str)]) -> Vec<u8> {
    class_bytes(name, 0x0021, instance, statics)
}

/// [`class_with_fields`] with the given class `access_flags`
/// (`0x0601` = `public interface abstract`).
fn class_bytes(
    name: &str,
    access_flags: u16,
    instance: &[(&str, &str)],
    statics: &[(&str, &str)],
) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let fields: Vec<(u16, &str, &str)> = instance
        .iter()
        .map(|&(n, d)| (0x0000, n, d))
        .chain(statics.iter().map(|&(n, d)| (0x0009, n, d)))
        .collect();
    let count = u16::try_from(fields.len()).unwrap_or(0);
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&(5 + 2 * count).to_be_bytes()); // constant_pool_count
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    for (_, n, d) in &fields {
        utf8(&mut b, n); // #5 + 2i
        utf8(&mut b, d); // #6 + 2i
    }
    b.extend_from_slice(&access_flags.to_be_bytes());
    b.extend_from_slice(&[0, 2, 0, 4, 0, 0]); // this, super, no interfaces
    b.extend_from_slice(&count.to_be_bytes()); // fields_count
    for (i, (flags, _, _)) in (0u16..).zip(&fields) {
        b.extend_from_slice(&flags.to_be_bytes());
        b.extend_from_slice(&(5 + 2 * i).to_be_bytes());
        b.extend_from_slice(&(6 + 2 * i).to_be_bytes());
        b.extend_from_slice(&[0, 0]); // no attributes
    }
    b.extend_from_slice(&[0, 0, 0, 0]); // no methods, no attributes
    b
}

/// Six instance fields `a:I b:Object c:J d:Z e:Object f:D` and one static
/// `manager:Object`: wider than the old 4-slot allocation.
fn six_field_class(name: &str) -> Vec<u8> {
    class_with_fields(name, &SIX_FIELDS, &[("manager", OBJ)])
}

const OBJ: &str = "Ljava/lang/Object;";
const SIX_FIELDS: [(&str, &str); 6] =
    [("a", "I"), ("b", OBJ), ("c", "J"), ("d", "Z"), ("e", OBJ), ("f", "D")];

fn define(shared: &SharedVm, name: &str, bytes: &[u8]) -> Option<ClassId> {
    define_in(shared, name, bytes, ClassLoaderId::Application)
}

fn define_in(shared: &SharedVm, name: &str, bytes: &[u8], loader: ClassLoaderId) -> Option<ClassId> {
    shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(name, bytes, loader, DefineClassOptions::default())
        .ok()
}

/// The object in `class_id`'s static number `idx`, if any.
fn static_object(shared: &SharedVm, class_id: ClassId, idx: usize) -> Option<ObjectRef> {
    match super::super::vm_object::get_static_shared(shared, class_id, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// The object in `class_id`'s first static after `post_clinit_fixup`
/// ran for `name`, and the same after a second run.
fn fixup_twice(shared: &SharedVm, class_id: ClassId, name: &str) -> (Option<ObjectRef>, Option<ObjectRef>) {
    let published = || static_object(shared, class_id, 0);
    super::post_clinit_fixup(shared, class_id, name);
    let first = published();
    super::post_clinit_fixup(shared, class_id, name);
    (first, published())
}

#[test]
fn a_fixup_instance_is_as_wide_as_its_class_and_holds_jvm_defaults() {
    const NAME: &str = "j27fixups/WideHolder";
    let shared = SharedVm::new(VmConfig::default());
    let Ok(cid) = shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            NAME,
            &six_field_class(NAME),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
    else {
        // The stripped test VM could not define it: nothing to run.
        return;
    };
    let obj = alloc_fixup_instance(&shared, cid).expect("a small object allocates");
    let (width, slot) = {
        let cm = shared.classes.class_manager.read();
        let width = cm.get_class(cid).map(|c| c.num_total_fields);
        let slot = |f: &str| {
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(cid, f, &cm.class_store)
                .expect("declared")
        };
        (width, [slot("a"), slot("b"), slot("c"), slot("e"), slot("f")])
    };
    assert!(width.is_some_and(|w| w >= 6), "six instance fields");
    assert_eq!(Some(shared.mem.heap.num_fields(obj)), width);
    let read = |i: usize| shared.mem.heap.get_field(obj, slot[i]);
    assert!(matches!(read(0), Value::Int(0)), "a:I");
    assert!(matches!(read(1), Value::Object(None)), "b:Object is null");
    assert!(matches!(read(2), Value::Long(0)), "c:J");
    assert!(matches!(read(3), Value::Object(None)), "e:Object, past slot 4");
    assert!(matches!(read(4), Value::Double(d) if d == 0.0), "f:D, past slot 4");
}

/// An arm that writes slot 0 by index keeps that slot on a class that
/// declares none, and the slot reads null, not `Int(0)`.
#[test]
fn a_minimum_width_pads_a_narrow_class_with_null_slots() {
    const NAME: &str = "j27fixups/Bare";
    let shared = SharedVm::new(VmConfig::default());
    let Some(cid) = define(&shared, NAME, &class_with_fields(NAME, &[], &[])) else {
        return;
    };
    let obj = alloc_fixup_instance_at_least(&shared, cid, 1).expect("allocates");
    assert_eq!(shared.mem.heap.num_fields(obj), 1);
    assert!(matches!(shared.mem.heap.get_field(obj, 0), Value::Object(None)));
    let wide = define(&shared, "j27fixups/Wide", &six_field_class("j27fixups/Wide"));
    if let Some(wide) = wide {
        let obj = alloc_fixup_instance_at_least(&shared, wide, 1).expect("allocates");
        assert!(shared.mem.heap.num_fields(obj) >= 6, "the layout wins over the minimum");
    }
}

/// The Quarkus arm's `QuarkusDelayedHandler` takes the handler's own
/// width (it was a fixed 4), and a second run keeps the published one.
#[test]
fn the_quarkus_handler_fixup_is_full_width_and_does_not_replace_a_published_handler() {
    const HOLDER: &str = "io/quarkus/bootstrap/logging/InitialConfigurator";
    const HANDLER: &str = "io/quarkus/bootstrap/logging/QuarkusDelayedHandler";
    let shared = SharedVm::new(VmConfig::default());
    let Some(handler) = define(&shared, HANDLER, &class_with_fields(HANDLER, &SIX_FIELDS, &[]))
    else {
        return;
    };
    let holder_bytes = class_with_fields(HOLDER, &[], &[("DELAYED_HANDLER", OBJ)]);
    let Some(holder) = define(&shared, HOLDER, &holder_bytes) else {
        return;
    };
    let (first, second) = fixup_twice(&shared, holder, HOLDER);
    let first = first.expect("DELAYED_HANDLER populated");
    assert_eq!(shared.mem.heap.class_id_of(first), handler);
    let width = shared.classes.class_manager.read().get_class(handler).map(|c| c.num_total_fields);
    assert!(width.is_some_and(|w| w >= 6));
    assert_eq!(Some(shared.mem.heap.num_fields(first)), width);
    assert_eq!(second, Some(first), "a published handler is not replaced");
}

/// The JBoss arm publishes a `LocalModuleLoader` at its layout width (it
/// was a literal 1, and a miss published the holder class), and a second
/// run keeps the published loader.
#[test]
fn the_jboss_holder_fixup_is_layout_width_and_does_not_replace_a_published_loader() {
    const HOLDER: &str = "org/jboss/modules/DefaultBootModuleLoaderHolder";
    const LOADER: &str = "org/jboss/modules/LocalModuleLoader";
    let shared = SharedVm::new(VmConfig::default());
    let Some(loader) = define(&shared, LOADER, &class_with_fields(LOADER, &SIX_FIELDS, &[])) else {
        return;
    };
    let holder_bytes = class_with_fields(HOLDER, &[], &[("INSTANCE", OBJ)]);
    let Some(holder) = define(&shared, HOLDER, &holder_bytes) else {
        return;
    };
    let (first, second) = fixup_twice(&shared, holder, HOLDER);
    let first = first.expect("INSTANCE populated");
    assert_eq!(shared.mem.heap.class_id_of(first), loader, "a LocalModuleLoader");
    let width = shared.classes.class_manager.read().get_class(loader).map(|c| c.num_total_fields);
    assert!(width.is_some_and(|w| w >= 6));
    assert_eq!(Some(shared.mem.heap.num_fields(first)), width);
    assert_eq!(second, Some(first), "a published loader is not replaced");
}

/// j27 wave 5 (lane CLINITFIX): a holder whose loader cannot see its
/// stand-in publishes nothing; both arms used to publish an instance of
/// the HOLDER class, which is not of the static's type. A user-defined
/// loader is the miss no load here can repair (it needs Java `loadClass`).
#[test]
fn a_holder_whose_loader_cannot_see_the_stand_in_publishes_nothing() {
    const FORK: ClassLoaderId = ClassLoaderId::UserDefined(27_051);
    let shared = SharedVm::new(VmConfig::default());
    for (holder_name, static_name) in [
        ("org/jboss/modules/DefaultBootModuleLoaderHolder", "INSTANCE"),
        ("io/quarkus/bootstrap/logging/InitialConfigurator", "DELAYED_HANDLER"),
    ] {
        let bytes = class_with_fields(holder_name, &[], &[(static_name, OBJ)]);
        let Some(holder) = define_in(&shared, holder_name, &bytes, FORK) else {
            continue;
        };
        let (first, second) = fixup_twice(&shared, holder, holder_name);
        assert_eq!(first, None, "{holder_name}.{static_name} stays null");
        assert_eq!(second, None, "{holder_name}.{static_name} stays null on a re-fire");
    }
}

/// The `ServiceLogger` arm never publishes an instance of an interface:
/// with `ServiceLogger_$logger` an interface (and, before, with it
/// missing, the arm allocated the interface `ServiceLogger` itself) the
/// three statics stay null.
#[test]
fn an_interface_stand_in_is_not_instantiated() {
    const HOLDER: &str = "org/jboss/msc/service/ServiceLogger";
    const IMPL: &str = "org/jboss/msc/service/ServiceLogger_$logger";
    let shared = SharedVm::new(VmConfig::default());
    if define(&shared, IMPL, &class_bytes(IMPL, 0x0601, &[], &[])).is_none() {
        return;
    }
    let statics = [("ROOT", OBJ), ("SERVICE", OBJ), ("FAIL", OBJ)];
    let Some(holder) = define(&shared, HOLDER, &class_with_fields(HOLDER, &[], &statics)) else {
        return;
    };
    super::post_clinit_fixup(&shared, holder, HOLDER);
    for idx in 0..3 {
        assert_eq!(static_object(&shared, holder, idx), None, "static {idx} stays null");
    }
}

/// The `ElytronMessages` arm's `$logger` takes the class's own width with
/// JVM defaults (it was a literal 2 slots) and is of the `$logger` class.
#[test]
fn the_elytron_logger_fixup_is_full_width_and_of_the_logger_class() {
    const HOLDER: &str = "org/wildfly/security/auth/server/_private/ElytronMessages";
    const IMPL: &str = "org/wildfly/security/auth/server/_private/ElytronMessages_$logger";
    let shared = SharedVm::new(VmConfig::default());
    let Some(logger) = define(&shared, IMPL, &class_with_fields(IMPL, &SIX_FIELDS, &[])) else {
        return;
    };
    let Some(holder) = define(&shared, HOLDER, &class_with_fields(HOLDER, &[], &[("log", OBJ)])) else {
        return;
    };
    let (first, second) = fixup_twice(&shared, holder, HOLDER);
    let first = first.expect("log populated");
    assert_eq!(shared.mem.heap.class_id_of(first), logger);
    let width = shared.classes.class_manager.read().get_class(logger).map(|c| c.num_total_fields);
    assert!(width.is_some_and(|w| w >= 6));
    assert_eq!(Some(shared.mem.heap.num_fields(first)), width);
    assert_eq!(second, Some(first), "a published logger is not replaced");
}
