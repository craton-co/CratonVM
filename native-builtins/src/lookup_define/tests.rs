use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

fn dummy_this() -> Value {
    Value::Object(None)
}

/// The EMPTY `ClassOption[]` a varargs call site actually passes. javac
/// emits `new ClassOption[0]` for `defineHiddenClass(bytes, initialize)`;
/// a null in that slot means the caller wrote `(ClassOption[]) null`, which
/// the JDK answers with NullPointerException. These tests used to pass a
/// null and so were exercising a frame shape no Java call site produces.
fn no_options(ctx: &mut MockNativeContext) -> cratonvm_types::ObjectRef {
    ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0)
}

/// Smallest valid class file prefix: magic + minor 0 + major 65 (JDK 21+).
/// The mock's `define_class_from_bytes` only checks the magic; downstream
/// linking / verification is skipped because of `skip_verification`.
fn cafebabe_minimal() -> Vec<u8> {
    cafebabe_named("Generated")
}

/// `cafebabe_minimal`'s class, in the package of `lookup_class_name`:
/// `Lookup.defineClass` / `defineHiddenClass` refuse a class of another
/// package (`lookup_define_refusal`).
fn cafebabe_beside(lookup_class_name: &str) -> Vec<u8> {
    match lookup_class_name.rsplit_once('/') {
        Some((pkg, _)) => cafebabe_named(&format!("{pkg}/Generated")),
        None => cafebabe_named("Generated"),
    }
}

/// The smallest class file naming `this_class` (internal form).
fn cafebabe_named(this_class: &str) -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE]; // magic
    b.extend_from_slice(&[0x00, 0x00]); // minor 0
    b.extend_from_slice(&[0x00, 0x41]); // major 65 (JDK 21)
    b.extend_from_slice(&[0x00, 0x05]); // cp_count = 5
    b.push(1); // #1 Utf8 this_class
    b.extend_from_slice(&(this_class.len() as u16).to_be_bytes());
    b.extend_from_slice(this_class.as_bytes());
    b.push(7); // #2 Class #1
    b.extend_from_slice(&[0x00, 0x01]);
    b.push(1); // #3 Utf8 java/lang/Object
    b.extend_from_slice(&[0x00, 0x10]);
    b.extend_from_slice(b"java/lang/Object");
    b.push(7); // #4 Class #3
    b.extend_from_slice(&[0x00, 0x03]);
    b.extend_from_slice(&[0x00, 0x21]); // access_flags = ACC_PUBLIC|ACC_SUPER
    b.extend_from_slice(&[0x00, 0x02]); // this_class
    b.extend_from_slice(&[0x00, 0x04]); // super_class
    b.extend_from_slice(&[0x00, 0x00]); // interfaces_count
    b.extend_from_slice(&[0x00, 0x00]); // fields_count
    b.extend_from_slice(&[0x00, 0x00]); // methods_count
    b.extend_from_slice(&[0x00, 0x00]); // attributes_count
    b
}

#[test]
fn lookup_define_class_rejects_null_bytes() {
    let mut ctx = MockNativeContext::new();
    let lookup = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(lookup, 1, Value::Int(0x5F)); // synthetic allowedModes: full power
    let r = lk_define_class_b(
        &mut ctx,
        &[Value::Object(Some(lookup)), Value::Object(None)],
    );
    // The TYPE is the contract here, and the previous assertion --
    // `is_err()` with a message naming an exception it never checked --
    // would have stayed green through the whole defect. `Lookup.defineClass`
    // is specified to throw NullPointerException for null bytes.
    assert!(
        matches!(
            r,
            Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    RuntimeError::NullPointerException { .. }
                )
            ))
        ),
        "null bytes must be NullPointerException, got {r:?}"
    );
}

#[test]
fn lookup_define_class_rejects_bad_magic() {
    let mut ctx = MockNativeContext::new();
    let lookup = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(lookup, 1, Value::Int(0x5F)); // synthetic allowedModes: full power
                                                // Allocate a byte[] with bad magic.
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    for i in 0..4 {
        ctx.set_array_element(bytes, i, Value::Int(0));
    }
    let r = lk_define_class_b(
        &mut ctx,
        &[Value::Object(Some(lookup)), Value::Object(Some(bytes))],
    );
    // Bad magic is a CLASS FORMAT problem, and `ClassFormatError` is an
    // `Error`: a generator guarding its emit with `catch (ClassFormatError)`
    // never sees an `IllegalArgumentException`, so the malformed class
    // escapes and fails somewhere unrelated.
    // Wave 94 (L5): the JDK's text needs a minted `ClassFormatError`
    // (`lookup_header_refusal`); the typed error is its fallback.
    let minted = matches!(r, Err(MethodCallFailed::ExceptionThrown(exc))
        if ctx.class_name_of_id(ctx.class_id_of_object(exc)).as_deref()
            == Some("java/lang/ClassFormatError"));
    assert!(
        minted
            || matches!(
                r,
                Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Linkage(
                        cratonvm_types::error::LinkageError::ClassFormatError { .. }
                    )
                ))
            ),
        "bad magic must be ClassFormatError, got {r:?}"
    );
}

/// A null `ClassOption[]` is a different argument from null bytes, and
/// `parse_nestmate_option` used to answer `false` for it -- silently
/// dropping a NESTMATE the caller asked for.
#[test]
fn define_hidden_class_rejects_null_options() {
    let mut ctx = MockNativeContext::new();
    let lookup = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(lookup, 1, Value::Int(0x5F)); // synthetic allowedModes: full power
    let class_bytes = cafebabe_minimal();
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes, i, Value::Int(*b as i32));
    }
    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_full(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(bytes)),
            Value::Int(0),
            Value::Object(None),
        ],
    );
    assert!(
        matches!(
            r,
            Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    RuntimeError::NullPointerException { .. }
                )
            ))
        ),
        "null options must be NullPointerException, got {r:?}"
    );
}

#[test]
fn lookup_define_class_succeeds_on_valid_magic() {
    let mut ctx = MockNativeContext::new();
    let lookup = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(lookup, 1, Value::Int(0x5F)); // synthetic allowedModes: full power
    let class_bytes = cafebabe_minimal();
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes, i, Value::Int(*b as i32));
    }
    let r = lk_define_class_b(
        &mut ctx,
        &[Value::Object(Some(lookup)), Value::Object(Some(bytes))],
    )
    .unwrap();
    // Result is the new Class mirror.
    assert!(matches!(r, Some(Value::Object(Some(_)))));
}

#[test]
fn define_hidden_class_returns_a_lookup() {
    let mut ctx = MockNativeContext::new();
    let lookup = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(lookup, 1, Value::Int(0x5F)); // synthetic allowedModes: full power
    let class_bytes = cafebabe_minimal();
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes, i, Value::Int(*b as i32));
    }
    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_full(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(bytes)),
            Value::Int(0), // initialize = false
            Value::Object(Some(opts)),
        ],
    )
    .unwrap();
    assert!(matches!(r, Some(Value::Object(Some(_)))));
}

/// gc-common w10-f: the class data lands in the new class's mirror, in
/// the `java.lang.Class.classData` field the JDK reads, not in the
/// write-only process-wide `ClassId -> ObjectRef` map it used to go to.
/// The mock mirror is class 0 with two slots; declaring `classData` at
/// slot 1 stands in for the real `java.lang.Class` layout.
#[test]
fn define_hidden_class_with_class_data_stores_it_in_the_mirror() {
    use cratonvm_native_api::FieldMetadata;
    let mut ctx = MockNativeContext::new();
    ctx.set_declared_fields(
        ClassId::new(0),
        vec![FieldMetadata {
            name: "classData".to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: 1,
            declaring_class_id: ClassId::new(0),
            is_static: false,
        }],
    );
    let lookup = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(lookup, 1, Value::Int(0x5F)); // synthetic allowedModes: full power
    let payload = ctx.alloc_object(ClassId::new(1), 1);
    let class_bytes = cafebabe_minimal();
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes, i, Value::Int(*b as i32));
    }
    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_with_class_data(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(bytes)),
            Value::Object(Some(payload)),
            Value::Int(0),
            Value::Object(Some(opts)),
        ],
    )
    .unwrap();
    let new_lookup = match r {
        Some(Value::Object(Some(l))) => l,
        other => panic!("expected a Lookup, got {other:?}"),
    };
    let mirror = match ctx.get_field(new_lookup, LK_LOOKUP_CLASS_REF) {
        Value::Object(Some(m)) => m,
        other => panic!("the new Lookup must carry the hidden class's mirror, got {other:?}"),
    };
    assert_eq!(
        ctx.get_field(mirror, 1),
        Value::Object(Some(payload)),
        "classData must be stored in the mirror's Class.classData field"
    );
}

// -----------------------------------------------------------------
// WP8.11.5 — NESTMATE flag propagation tests
// -----------------------------------------------------------------
//
// These tests pin the JEP 371 / JLS §12.7 contract for hidden-class
// nest-host inheritance. The WP8.11 diagnostic agent flagged that
// EJBCA's Weld CDI `Bean<?>` proxy generation was failing every
// `@Inject` injection point with `IllegalAccessError` because the
// NESTMATE option from `defineHiddenClass(bytes, true, NESTMATE,
// STRONG)` was being collapsed to a no-op rather than propagated to
// `define_class_full`'s `nest_host_class_name` field.
//
// The fix in this file walks the ClassOption[] varargs and only
// sets `nest_host_class_name` when NESTMATE is requested; the
// resolved name is the *lookup class's nest host* (not the lookup
// class itself) so a hidden class defined from inside an
// `Outer$Inner` lookup correctly joins `Outer`'s nest.

/// Build the smallest valid `MethodHandles$Lookup$ClassOption` enum
/// mirror with the requested ordinal at field 0 (NESTMATE = 0,
/// STRONG = 1). Mirrors the convention used by
/// `classloader.rs::lk_define_hidden_class`.
fn make_class_option(ctx: &mut MockNativeContext, ordinal: i32) -> ObjectRef {
    let opt_cid = ctx
        .ensure_class_initialized("java/lang/invoke/MethodHandles$Lookup$ClassOption")
        .expect("alloc class option cid");
    let opt = ctx.alloc_object(opt_cid, 4);
    ctx.set_field(opt, 0, Value::Int(ordinal));
    opt
}

/// Build a `ClassOption[]` array containing the given ordinals.
fn make_options_array(ctx: &mut MockNativeContext, ordinals: &[i32]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, ordinals.len());
    for (i, ord) in ordinals.iter().enumerate() {
        let opt = make_class_option(ctx, *ord);
        ctx.set_array_element(arr, i, Value::Object(Some(opt)));
    }
    arr
}

/// Allocate a Lookup whose lookup-class field points at a mirror of
/// `class_name`, registering the name under a fresh ClassId.
/// Returns `(lookup_object, lookup_class_id)`.
fn make_lookup_for(ctx: &mut MockNativeContext, class_name: &str) -> (ObjectRef, ClassId) {
    let cid = ctx.ensure_class_initialized(class_name).expect("alloc cid");
    let mirror = ctx.get_class_mirror(cid);
    let lookup = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(lookup, 1, Value::Int(0x5F)); // synthetic allowedModes: full power
    ctx.set_field(lookup, LK_LOOKUP_CLASS_REF, Value::Object(Some(mirror)));
    (lookup, cid)
}

/// Drive `defineHiddenClass([B, true, options...)` with the given
/// lookup and ClassOption ordinals, returning the captured
/// `DefineClassFull.nest_host_class_name` actually passed to the
/// backend. Asserts the call itself succeeded.
fn drive_hidden_define(
    lookup_class_name: &str,
    lookup_nest_host: Option<&str>,
    option_ordinals: &[i32],
) -> Option<String> {
    let mut ctx = MockNativeContext::new();
    let (lookup, lookup_cid) = make_lookup_for(&mut ctx, lookup_class_name);
    if let Some(host) = lookup_nest_host {
        ctx.set_nest_host_override(lookup_cid, host);
    }

    let class_bytes = cafebabe_beside(lookup_class_name);
    let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
    }

    let opts_arr = make_options_array(&mut ctx, option_ordinals);

    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_full(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(bytes_arr)),
            Value::Int(0), // initialize = false
            Value::Object(Some(opts_arr)),
        ],
    );
    assert!(r.is_ok(), "defineHiddenClass must succeed: {:?}", r.err());

    ctx.last_define_full_opts()
        .expect("define_class_full was not invoked")
        .nest_host_class_name
}

/// Acceptance #1: when the lookup class IS its own nest host
/// (no `NestHost` attribute) and NESTMATE is requested, the hidden
/// class's nest host is the lookup class itself.
#[test]
fn nestmate_propagates_when_lookup_is_own_nest_host() {
    let nh = drive_hidden_define(
        "weld/cdi/BeanFactory",
        None, // lookup IS its own nest host
        &[0], // NESTMATE
    );
    assert_eq!(
        nh.as_deref(),
        Some("weld/cdi/BeanFactory"),
        "NESTMATE-only hidden class must inherit the lookup class as nest host"
    );
}

/// Acceptance #2: when the lookup class is itself a nestmate of an
/// outer class (i.e. its own `NestHost` attribute names the outer),
/// the hidden class's nest host is the OUTER class — not the
/// (already-nested) lookup class. This is the JEP 371 / JLS §12.7
/// transitive case that broke EJBCA's `Bean<?>` proxies: Weld's
/// `BeanFactory` is a static inner of `BeanManagerImpl`, so the
/// proxy must end up in `BeanManagerImpl`'s nest, not in
/// `BeanFactory`'s (which would not even be a valid nest).
#[test]
fn nestmate_inherits_outer_when_lookup_is_a_nestmate() {
    let nh = drive_hidden_define(
        "weld/cdi/BeanManagerImpl$BeanFactory",
        Some("weld/cdi/BeanManagerImpl"), // lookup is itself a nestmate
        &[0],                             // NESTMATE
    );
    assert_eq!(
        nh.as_deref(),
        Some("weld/cdi/BeanManagerImpl"),
        "NESTMATE on a nestmate-lookup must resolve to the OUTER nest host \
         (JEP 371: hidden class joins lookup class's nest, not the lookup itself)"
    );
}

/// Acceptance #3: when NESTMATE is absent from the ClassOption[]
/// array (e.g. only STRONG passed, or empty options), the backend
/// receives `nest_host_class_name = None` so the class file's own
/// `NestHost` attribute (or self-nest) determines the nest host.
/// This guarantees we don't regress the default case where a
/// hidden class should remain in its own nest.
#[test]
fn no_nestmate_means_no_nest_host_inheritance() {
    // Empty options.
    let nh_empty = drive_hidden_define("some/Caller", None, &[]);
    assert_eq!(
        nh_empty, None,
        "empty ClassOption[] must NOT inherit nest host"
    );

    // STRONG-only (ordinal 1) — also must not trigger inheritance.
    let nh_strong = drive_hidden_define("some/Caller", None, &[1]);
    assert_eq!(
        nh_strong, None,
        "STRONG-only ClassOption[] must NOT inherit nest host"
    );
}

/// Defence-in-depth: NESTMATE + STRONG together (the exact pattern
/// emitted by Weld CDI's proxy generator) propagates correctly.
/// This pins the original Weld bytecode pattern from the WP8.11
/// diagnostic.
#[test]
fn nestmate_plus_strong_still_propagates() {
    let nh = drive_hidden_define(
        "weld/cdi/BeanManagerImpl$BeanFactory",
        Some("weld/cdi/BeanManagerImpl"),
        &[0, 1], // NESTMATE, STRONG — exact Weld pattern
    );
    assert_eq!(
        nh.as_deref(),
        Some("weld/cdi/BeanManagerImpl"),
        "NESTMATE + STRONG (Weld's exact ClassOption[] pattern) must \
         propagate the outer nest host"
    );
}

// -----------------------------------------------------------------
// W3-2 — the REAL-JDK ClassOption layout
// -----------------------------------------------------------------
//
// Every test above builds a SYNTHETIC ClassOption whose field 0 holds the
// ordinal. Under `--fast-boot` the varargs array carries genuine JDK enum
// constants instead, and `java.lang.Enum` declares `name` before
// `ordinal`, so field 0 is a String. Reading it as an `i32` silently
// answered "not a nestmate" for NESTMATE too — see
// `class_option_is_nestmate` for why that stayed invisible.

/// Build a `ClassOption` shaped like a REAL JDK enum constant: slot 0
/// carries `Enum.name` (a String), NOT the ordinal. `set_field_by_name`
/// additionally places the name wherever the mock's field model says
/// `name` lives, and slot 0 is deliberately a String so the synthetic
/// ordinal fallback CANNOT be what answers.
fn make_real_enum_class_option(ctx: &mut MockNativeContext, const_name: &str) -> ObjectRef {
    let opt_cid = ctx
        .ensure_class_initialized("java/lang/invoke/MethodHandles$Lookup$ClassOption")
        .expect("alloc class option cid");
    let opt = ctx.alloc_object(opt_cid, 4);
    let name_obj = ctx.create_string(const_name);
    ctx.set_field(opt, 0, Value::Object(Some(name_obj)));
    ctx.set_field_by_name(opt, "name", Value::Object(Some(name_obj)));
    opt
}

/// As `drive_hidden_define`, but the ClassOption[] elements use the
/// real-JDK enum layout. Returns the captured `nest_host_class_name`.
fn drive_hidden_define_real_enum_options(
    lookup_class_name: &str,
    const_names: &[&str],
) -> Option<String> {
    let mut ctx = MockNativeContext::new();
    let (lookup, _lookup_cid) = make_lookup_for(&mut ctx, lookup_class_name);

    let class_bytes = cafebabe_beside(lookup_class_name);
    let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
    }

    let opts_arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Reference,
        const_names.len(),
    );
    for (i, n) in const_names.iter().enumerate() {
        let opt = make_real_enum_class_option(&mut ctx, n);
        ctx.set_array_element(opts_arr, i, Value::Object(Some(opt)));
    }

    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_full(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(bytes_arr)),
            Value::Int(0), // initialize = false
            Value::Object(Some(opts_arr)),
        ],
    );
    assert!(r.is_ok(), "defineHiddenClass must succeed: {:?}", r.err());
    ctx.last_define_full_opts()
        .expect("define_class_full was not invoked")
        .nest_host_class_name
}

/// THE REAL-JDK NESTMATE ARM. FAILS BEFORE THE W3-2 FIX.
///
/// `RJdkHidden.java:101` appeared to pass only because the *class file's*
/// `NestHost` attribute happened to name the same class the NESTMATE
/// option would have selected. Assert the option itself is decoded, so
/// the answer no longer depends on that coincidence.
#[test]
fn real_jdk_enum_class_option_is_decoded_as_nestmate() {
    let nh = drive_hidden_define_real_enum_options("RJdkHidden", &["NESTMATE"]);
    assert_eq!(
        nh.as_deref(),
        Some("RJdkHidden"),
        "a real-JDK ClassOption enum constant stores its NAME in slot 0; \
         NESTMATE must still be recognised"
    );
}

/// Control: the real-JDK layout must not turn STRONG into a nestmate.
/// `Enum.ordinal` for STRONG is 1 and its `flag` is 0x4 — neither may be
/// mistaken for the NESTMATE bit.
#[test]
fn real_jdk_enum_strong_only_is_not_a_nestmate() {
    let nh = drive_hidden_define_real_enum_options("RJdkHidden", &["STRONG"]);
    assert_eq!(
        nh, None,
        "STRONG alone must leave the hidden class in its own nest"
    );
}

/// The Weld/LambdaMetafactory pattern under the real-JDK layout: order
/// must not matter, and STRONG must not veto NESTMATE.
#[test]
fn real_jdk_enum_strong_then_nestmate_still_propagates() {
    let nh = drive_hidden_define_real_enum_options("RJdkHidden", &["STRONG", "NESTMATE"]);
    assert_eq!(nh.as_deref(), Some("RJdkHidden"));
}

/// Defence-in-depth: the WithClassData variant (used by
/// LambdaMetafactory and Weld's classData-bound proxies) also walks
/// the ClassOption[] correctly. We only need a single happy-path
/// assertion since the helper functions are shared with the plain
/// variant.
#[test]
fn with_class_data_also_propagates_nestmate() {
    let mut ctx = MockNativeContext::new();
    let (lookup, lookup_cid) =
        make_lookup_for(&mut ctx, "weld/cdi/BeanManagerImpl$BeanFactory");
    ctx.set_nest_host_override(lookup_cid, "weld/cdi/BeanManagerImpl");

    let class_bytes = cafebabe_beside("weld/cdi/BeanManagerImpl$BeanFactory");
    let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
    }
    let payload = ctx.alloc_object(ClassId::new(1), 1);
    let opts_arr = make_options_array(&mut ctx, &[0, 1]);

    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_with_class_data(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(bytes_arr)),
            Value::Object(Some(payload)),
            Value::Int(0),
            Value::Object(Some(opts_arr)),
        ],
    );
    assert!(r.is_ok(), "defineHiddenClassWithClassData must succeed");

    let captured = ctx
        .last_define_full_opts()
        .expect("define_class_full not invoked")
        .nest_host_class_name;
    assert_eq!(
        captured.as_deref(),
        Some("weld/cdi/BeanManagerImpl"),
        "WithClassData variant must also resolve to the outer nest host"
    );
}

// -----------------------------------------------------------------
// W6-3 — the REAL-JDK `MethodHandles$Lookup` layout
// -----------------------------------------------------------------

/// `javap -p java.lang.invoke.MethodHandles$Lookup` on JDK 25, instance
/// fields in declaration order:
///
/// ```text
///   0 lookupClass            (ref)  private final Class<?>
///   1 prevLookupClass        (ref)  private final Class<?>
///   2 allowedModes           (int)  private final int
///   3 cachedProtectionDomain (ref)  private volatile ProtectionDomain
/// ```
///
/// The synthetic layout disagrees from slot 1 onwards
/// (`lookupClass | allowedModes | previousLookupClass | lookupMode`), so
/// writing modes BY INDEX type-confused all three: `Int(0x5F)` landed in the
/// `prevLookupClass` REFERENCE slot, a null ref landed in `allowedModes` —
/// which every JDK access check reads as 0, "no access at all" — and another
/// `Int(0x5F)` landed in the `cachedProtectionDomain` reference slot. None of
/// the three threw anything; the only symptom was a powerless Lookup.
///
/// Declaring the real field names on the mock's class makes the by-name arm
/// the one that runs, which is what this asserts.
#[test]
fn alloc_lookup_for_writes_real_jdk_layout_by_name() {
    use cratonvm_native_api::FieldMetadata;
    let mut ctx = MockNativeContext::new();
    let lk_cid = ctx.ensure_class_initialized(LK_CLASS).expect("lookup cid");
    let fm = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: lk_cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        lk_cid,
        vec![
            fm("lookupClass", "Ljava/lang/Class;", 0),
            fm("prevLookupClass", "Ljava/lang/Class;", 1),
            fm("allowedModes", "I", 2),
            fm(
                "cachedProtectionDomain",
                "Ljava/security/ProtectionDomain;",
                3,
            ),
        ],
    );

    let host_cid = ctx.ensure_class_initialized("p/Host").expect("host cid");
    let mirror = ctx.get_class_mirror(host_cid);
    let lookup = alloc_lookup_for(&mut ctx, mirror).unwrap();

    assert!(
        matches!(ctx.get_field(lookup, 0), Value::Object(Some(m)) if m == mirror),
        "lookupClass is slot 0 in BOTH layouts"
    );
    assert_ne!(
        ctx.get_field(lookup, 1),
        Value::Int(0x5F),
        "slot 1 is the `prevLookupClass` REFERENCE — the mode word must not land here"
    );
    assert_eq!(
        ctx.get_field(lookup, 2),
        Value::Int(0x5F),
        "allowedModes must be FULL_POWER_MODES (95 — measured on JDK 25 as \
         `MethodHandles.lookup().lookupModes()`); a null here reads back as 0, \
         which is `no access at all`"
    );
    assert_ne!(
        ctx.get_field(lookup, 3),
        Value::Int(0x5F),
        "cachedProtectionDomain is a lazy volatile REFERENCE cache; it must stay \
         null rather than receive an integer the GC would scan as an oop"
    );
}

/// The negative half: with NO real field names declared, the object is a
/// fabricated stub (`_f0.._f3`), the by-name lookups all miss, and the
/// synthetic indices must still be written — `allowedModes` at slot 1.
/// A by-name-only fix would have left this arm powerless.
///
/// This test was GREEN while production took the opposite arm. The mock's
/// `get_field_by_name` answers `Int(0)` for an unresolvable name;
/// production answers `Value::Object(None)`, which the discriminator's old
/// `matches!(.., Value::Object(_))` disjunct accepted as proof of the real
/// layout. The discriminator is now the class-side witness alone, which
/// both contexts answer identically, so this assertion means in production
/// what it means here.
#[test]
fn alloc_lookup_for_still_writes_the_synthetic_indices() {
    let mut ctx = MockNativeContext::new();
    let host_cid = ctx.ensure_class_initialized("p/Host").expect("host cid");
    let mirror = ctx.get_class_mirror(host_cid);
    let lookup = alloc_lookup_for(&mut ctx, mirror).unwrap();
    assert!(
        matches!(ctx.get_field(lookup, 0), Value::Object(Some(m)) if m == mirror),
        "synthetic slot 0 is lookupClass"
    );
    assert_eq!(
        ctx.get_field(lookup, 1),
        Value::Int(0x5F),
        "synthetic slot 1 is allowedModes"
    );
    assert_eq!(
        ctx.get_field(lookup, 2),
        Value::Object(None),
        "synthetic slot 2 is previousLookupClass"
    );
}

/// JDK 25's refusals before a define (`lookup_define_refusal`): no
/// `PACKAGE` access, a class of another package, and a hidden define
/// without full privilege access. Nothing reaches the backend.
#[test]
fn a_lookup_define_is_refused_as_the_jdk_refuses_it() {
    fn bytes_of(ctx: &mut MockNativeContext, class_bytes: &[u8]) -> ObjectRef {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        arr
    }
    let illegal_access = |r: &MethodCallResult| {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    RuntimeError::IllegalAccessException { .. }
                )
            ))
        )
    };
    let illegal_argument = |r: &MethodCallResult, text: &str| {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { message }
            ))) if message == text
        )
    };

    let mut ctx = MockNativeContext::new();
    let (lookup, _) = make_lookup_for(&mut ctx, "p/Host");
    // `dropLookupMode(PACKAGE)`: PUBLIC | MODULE.
    ctx.set_field(lookup, 1, Value::Int(0x11));
    let arr = bytes_of(&mut ctx, &cafebabe_named("p/Generated"));
    let r = lk_define_class_b(
        &mut ctx,
        &[Value::Object(Some(lookup)), Value::Object(Some(arr))],
    );
    assert!(illegal_access(&r), "no PACKAGE access: {r:?}");

    let opts = no_options(&mut ctx);
    let arr = bytes_of(&mut ctx, &cafebabe_named("p/Generated"));
    let r = lk_define_hidden_class_full(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Object(Some(opts)),
        ],
    );
    assert!(illegal_access(&r), "no full privilege access: {r:?}");

    ctx.set_field(lookup, 1, Value::Int(0x5F));
    let arr = bytes_of(&mut ctx, &cafebabe_named("q/Generated"));
    let r = lk_define_class_b(
        &mut ctx,
        &[Value::Object(Some(lookup)), Value::Object(Some(arr))],
    );
    assert!(
        illegal_argument(&r, "q/Generated not in same package as lookup class"),
        "another package: {r:?}"
    );
    assert!(ctx.last_define_full_opts().is_none(), "nothing was defined");
}

/// Smoke: keep the unused `dummy_this` helper alive so editors don't
/// flag it; future negative tests may want a null-this lookup.
#[test]
fn dummy_this_is_null_object() {
    assert!(matches!(dummy_this(), Value::Object(None)));
}

// -----------------------------------------------------------------
// CGLIB-η — Loader inheritance tests
// -----------------------------------------------------------------
//
// CGLIB 3.4+ / Spring 5.x emit proxies via `Lookup.defineClass` and
// `Lookup.defineHiddenClass`. After defining, CGLIB does
// `Class.forName(name, false, hostLoader)` to round-trip back to the
// mirror — which only works if the new class lives in the lookup
// class's loader namespace, NOT the application loader. Prior to the
// CGLIB-η fix, both natives passed `loader_id = 0` (Application)
// unconditionally, so any class defined via a Lookup whose host was
// in a user-defined loader (Spring Boot's LaunchedURLClassLoader,
// OSGi bundle loaders, web-app WAR loaders) would be lost.
//
// These tests pin that the lookup class's loader is inherited by:
//   1. `Lookup.defineClass(byte[])` (the normal-class variant);
//   2. `Lookup.defineHiddenClass(...)`;
//   3. `Lookup.defineHiddenClassWithClassData(...)`.
//
// And that built-in loaders (Bootstrap/Extension/Application) all
// still collapse to `loader_id = 0` so we don't regress the common
// case.

/// Acceptance #1: a user-defined loader (id >= 3) on the lookup class
/// must be inherited by `Lookup.defineClass`.
#[test]
fn define_class_inherits_user_defined_loader() {
    let mut ctx = MockNativeContext::new();
    let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "spring/boot/Service");
    // Spring Boot's LaunchedURLClassLoader gets a user-defined id.
    ctx.set_loader_id_override(lookup_cid, 7);

    let class_bytes = cafebabe_beside("spring/boot/Service");
    let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
    }
    let r = lk_define_class_b(
        &mut ctx,
        &[Value::Object(Some(lookup)), Value::Object(Some(bytes_arr))],
    );
    assert!(r.is_ok(), "defineClass must succeed: {:?}", r.err());
    assert_eq!(
        ctx.last_define_full_loader(),
        Some(7),
        "Lookup.defineClass must inherit the lookup class's user-defined loader"
    );
    let opts = ctx
        .last_define_full_opts()
        .expect("Lookup.defineClass must pass full definition options");
    assert!(
        opts.force_loader_faithful_linking,
        "generated classes must request loader-faithful linking"
    );
    assert!(
        opts.superclass_id_override.is_some(),
        "the superclass resolved through the lookup class must be preserved by identity"
    );
    assert!(
        opts.interface_id_overrides.is_some(),
        "the complete interface identity list must be preserved, including an empty list"
    );
}

/// Acceptance #2: Application loader (raw id 2) collapses to backend
/// loader_id 0, and so does Bootstrap (0). Extension (1) keeps the
/// platform loader's own namespace since zt t25/df12.
#[test]
fn define_class_built_in_loaders_collapse_to_zero() {
    for raw in [0_i32, 1, 2] {
        let mut ctx = MockNativeContext::new();
        let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "java/util/HashMap");
        ctx.set_loader_id_override(lookup_cid, raw);

        let class_bytes = cafebabe_beside("java/util/HashMap");
        let bytes_arr =
            ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
        }
        let r = lk_define_class_b(
            &mut ctx,
            &[Value::Object(Some(lookup)), Value::Object(Some(bytes_arr))],
        );
        assert!(r.is_ok(), "defineClass must succeed for raw={raw}");
        // zt t25/df12: a platform-loader lookup class (raw 1, `Extension`) defines in
        // the platform loader's own namespace while its switch is on.
        let want = if raw == 1 && crate::lang_system::define_platform_loader_own_namespace() {
            1
        } else {
            0
        };
        assert_eq!(
            ctx.last_define_full_loader(),
            Some(want),
            "raw loader {raw} (built-in) must define in backend loader_id {want}"
        );
    }
}

/// Acceptance #3: `Lookup.defineHiddenClass(...)` inherits the
/// user-defined loader. CGLIB 3.4+ relies on this so the hidden
/// proxy is loader-visible to its host.
#[test]
fn define_hidden_class_inherits_user_defined_loader() {
    let mut ctx = MockNativeContext::new();
    let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "cglib/Enhancer");
    ctx.set_loader_id_override(lookup_cid, 11);

    let class_bytes = cafebabe_beside("cglib/Enhancer");
    let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
    }
    let opts_arr = make_options_array(&mut ctx, &[0]); // NESTMATE
    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_full(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(bytes_arr)),
            Value::Int(0),
            Value::Object(Some(opts_arr)),
        ],
    );
    assert!(r.is_ok(), "defineHiddenClass must succeed: {:?}", r.err());
    assert_eq!(
        ctx.last_define_full_loader(),
        Some(11),
        "Lookup.defineHiddenClass must inherit the lookup class's user-defined loader"
    );
    let opts = ctx
        .last_define_full_opts()
        .expect("defineHiddenClass must pass full definition options");
    assert!(opts.force_loader_faithful_linking);
    assert!(opts.superclass_id_override.is_some());
    assert!(opts.interface_id_overrides.is_some());
}

/// Acceptance #4: `Lookup.defineHiddenClassWithClassData(...)` also
/// inherits the user-defined loader — LambdaMetafactory and CGLIB
/// classData-bound proxies both rely on this.
#[test]
fn define_hidden_class_with_class_data_inherits_loader() {
    let mut ctx = MockNativeContext::new();
    let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "lambda/Host");
    ctx.set_loader_id_override(lookup_cid, 42);
    let payload = ctx.alloc_object(ClassId::new(1), 1);

    let class_bytes = cafebabe_beside("lambda/Host");
    let bytes_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes_arr, i, Value::Int(*b as i32));
    }
    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_with_class_data(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(bytes_arr)),
            Value::Object(Some(payload)),
            Value::Int(0),
            Value::Object(Some(opts)),
        ],
    );
    assert!(r.is_ok(), "defineHiddenClassWithClassData must succeed");
    assert_eq!(
        ctx.last_define_full_loader(),
        Some(42),
        "Lookup.defineHiddenClassWithClassData must inherit the user-defined loader"
    );
    let opts = ctx
        .last_define_full_opts()
        .expect("defineHiddenClassWithClassData must pass full definition options");
    assert!(opts.force_loader_faithful_linking);
    assert!(opts.superclass_id_override.is_some());
    assert!(opts.interface_id_overrides.is_some());
}

/// gc-common w10-f: a hidden class defined through a lookup on a class
/// with a recorded (user) defining loader records that loader too, from
/// both hidden-class natives, so it unloads with the loader and its
/// instances pin it (`loader_pin` is built from the same rows). A lookup
/// class with no recorded loader records nothing, as before.
#[test]
fn hidden_classes_record_the_lookup_classes_defining_loader() {
    const VM: usize = 0x10F_3A00;
    fn hidden_class_id(ctx: &mut MockNativeContext, result: MethodCallResult) -> ClassId {
        let new_lookup = match result {
            Ok(Some(Value::Object(Some(l)))) => l,
            other => panic!("expected a Lookup, got {other:?}"),
        };
        let mirror = match ctx.get_field(new_lookup, LK_LOOKUP_CLASS_REF) {
            Value::Object(Some(m)) => m,
            other => panic!("expected the hidden class's mirror, got {other:?}"),
        };
        crate::lang_class::mirror_class_id(&*ctx, mirror).expect("the mirror names its class")
    }
    fn bytes(ctx: &mut MockNativeContext) -> ObjectRef {
        let class_bytes = cafebabe_beside("w10f/UserHost");
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        arr
    }
    if !crate::classloader::loader_aware_resolution() {
        eprintln!("skipped: loader-aware resolution is off in this process");
        return;
    }

    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let (lookup, lookup_cid) = make_lookup_for(&mut ctx, "w10f/UserHost");
    let loader = ctx.fresh_object_ref();
    crate::classloader::register_defining_loader(crate::classloader::detached_loader_tables(VM), lookup_cid.as_u32(), loader);

    let arr = bytes(&mut ctx);
    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_full(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Object(Some(opts)),
        ],
    );
    let plain = hidden_class_id(&mut ctx, r);
    assert_eq!(
        crate::classloader::defining_loader_for(crate::classloader::detached_loader_tables(VM), plain.as_u32()),
        Some(loader),
        "defineHiddenClass must record the lookup class's defining loader"
    );

    let arr = bytes(&mut ctx);
    let opts = no_options(&mut ctx);
    let payload = ctx.fresh_object_ref();
    let r = lk_define_hidden_class_with_class_data(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(arr)),
            Value::Object(Some(payload)),
            Value::Int(0),
            Value::Object(Some(opts)),
        ],
    );
    let with_data = hidden_class_id(&mut ctx, r);
    assert_eq!(
        crate::classloader::defining_loader_for(crate::classloader::detached_loader_tables(VM), with_data.as_u32()),
        Some(loader),
        "defineHiddenClassWithClassData must record it too"
    );

    // Control: a lookup class with no recorded loader.
    let (app_lookup, _) = make_lookup_for(&mut ctx, "w10f/AppHost");
    let arr = bytes(&mut ctx);
    let opts = no_options(&mut ctx);
    let r = lk_define_hidden_class_full(
        &mut ctx,
        &[
            Value::Object(Some(app_lookup)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Object(Some(opts)),
        ],
    );
    let app_hidden = hidden_class_id(&mut ctx, r);
    assert_eq!(
        crate::classloader::defining_loader_for(crate::classloader::detached_loader_tables(VM), app_hidden.as_u32()),
        None,
        "a hidden class of an unrecorded lookup class records nothing"
    );

    crate::classloader::forget_vm_loader_singletons(VM);
}

/// gc-common w18-d
/// (`common-w8e-non-strong-hidden-classes-unload-only-with-their-loader`):
/// `ClassOption.STRONG` is decoded, not advisory. Without it the define
/// carries `non_strong_hidden` and the class is registered to unload with
/// its mirror: its `loader_pin` row -- the collectors' instance edge --
/// names the MIRROR, so a live instance keeps the class. With it (alone or
/// beside NESTMATE, the lambda-proxy shape) nothing is registered and the
/// class keeps its loader's lifetime. Both natives, both layouts' ordinals.
#[test]
fn w18d_strong_decides_whether_a_hidden_class_unloads_with_its_mirror() {
    const VM: usize = 0x18D_0A00;
    fn define(
        ctx: &mut MockNativeContext,
        lookup: ObjectRef,
        ordinals: &[i32],
        with_data: bool,
    ) -> (bool, ClassId) {
        let class_bytes = cafebabe_beside("w18d/AppHost");
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        let opts = make_options_array(ctx, ordinals);
        let r = if with_data {
            let payload = ctx.fresh_object_ref();
            lk_define_hidden_class_with_class_data(
                ctx,
                &[
                    Value::Object(Some(lookup)),
                    Value::Object(Some(arr)),
                    Value::Object(Some(payload)),
                    Value::Int(0),
                    Value::Object(Some(opts)),
                ],
            )
        } else {
            lk_define_hidden_class_full(
                ctx,
                &[
                    Value::Object(Some(lookup)),
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Object(Some(opts)),
                ],
            )
        };
        let new_lookup = match r {
            Ok(Some(Value::Object(Some(l)))) => l,
            other => panic!("expected a Lookup, got {other:?}"),
        };
        let mirror = match ctx.get_field(new_lookup, LK_LOOKUP_CLASS_REF) {
            Value::Object(Some(m)) => m,
            other => panic!("expected the hidden class's mirror, got {other:?}"),
        };
        let cid = crate::lang_class::mirror_class_id(&*ctx, mirror)
            .expect("the mirror names its class");
        let non_strong = ctx
            .last_define_full_opts()
            .expect("define_class_full was invoked")
            .non_strong_hidden;
        (non_strong, cid)
    }

    if !crate::classloader::loader_unload_enabled() {
        eprintln!("skipped: CRATONVM_LOADER_UNLOAD=0 unloads nothing");
        return;
    }
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let (lookup, _) = make_lookup_for(&mut ctx, "w18d/AppHost");
    for with_data in [false, true] {
        for (ordinals, expect_non_strong) in [
            (&[][..], true),
            (&[0][..], true),
            (&[1][..], false),
            (&[0, 1][..], false),
        ] {
            let (non_strong, cid) = define(&mut ctx, lookup, ordinals, with_data);
            assert_eq!(
                non_strong, expect_non_strong,
                "options {ordinals:?} (with_data={with_data}): non_strong_hidden"
            );
            let mirror = crate::classloader::non_strong_hidden_mirror(crate::classloader::detached_loader_tables(VM), cid.as_u32());
            if expect_non_strong {
                // (The mock mints a fresh mirror object per
                // `get_class_mirror`, so the registered address is compared
                // with the `loader_pin` row, not with a new mirror.)
                assert!(
                    mirror.is_some(),
                    "options {ordinals:?}: a non-strong hidden class is registered"
                );
                assert_eq!(
                    cratonvm_types::loader_pin::loader_pin_addr_for_vm(VM, cid.as_u32()),
                    mirror,
                    "its instances must keep its MIRROR alive"
                );
            } else {
                assert_eq!(
                    mirror, None,
                    "options {ordinals:?}: STRONG stays loader-lifetime"
                );
                assert_eq!(
                    cratonvm_types::loader_pin::loader_pin_addr_for_vm(VM, cid.as_u32()),
                    None
                );
            }
        }
    }
    crate::classloader::forget_vm_loader_singletons(VM);
}

/// zs s10/nd9 (`zgc-zs9rv6-wave-s8-review-residuals-FIXED-20261019.md` item
/// 4): spent-ladder rows for this file's three definers
/// (`test_utils::spent_define_row`): every mirror reservation refused and
/// the reclaim ladder empty -> `OutOfMemoryError` with `define_class_full`
/// never reached and no pin left; with room the body defines. Kill
/// control: `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER=0`.
mod zs10nd9_define_rows {
    use super::*;
    use crate::test_utils::{assert_kill_row, assert_reserved_row, spent_define_row};

    const HOST: &str = "zs10nd9/ld/Host";

    fn bytes_array(ctx: &mut MockNativeContext) -> ObjectRef {
        let class_bytes = cafebabe_beside(HOST);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        arr
    }

    fn define_class_b_row(kill: bool) -> crate::test_utils::SpentDefineRow {
        let mut ctx = MockNativeContext::new();
        let (lookup, _) = make_lookup_for(&mut ctx, HOST);
        let arr = bytes_array(&mut ctx);
        spent_define_row(&mut ctx, kill, |ctx| {
            lk_define_class_b(
                ctx,
                &[Value::Object(Some(lookup)), Value::Object(Some(arr))],
            )
        })
    }

    #[test]
    fn zs10nd9_lookup_define_class_fails_before_it_publishes() {
        assert_reserved_row("lk_define_class_b_pinned", &define_class_b_row(false));
    }

    #[test]
    fn zs10nd9_lookup_define_class_kill_defines_first() {
        assert_kill_row("lk_define_class_b_pinned", &define_class_b_row(true));
    }

    fn hidden_full_row(kill: bool) -> crate::test_utils::SpentDefineRow {
        let mut ctx = MockNativeContext::new();
        let (lookup, _) = make_lookup_for(&mut ctx, HOST);
        let arr = bytes_array(&mut ctx);
        let opts = no_options(&mut ctx);
        spent_define_row(&mut ctx, kill, |ctx| {
            lk_define_hidden_class_full(
                ctx,
                &[
                    Value::Object(Some(lookup)),
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Object(Some(opts)),
                ],
            )
        })
    }

    #[test]
    fn zs10nd9_lookup_define_hidden_class_fails_before_it_publishes() {
        assert_reserved_row("lk_define_hidden_class_full", &hidden_full_row(false));
    }

    #[test]
    fn zs10nd9_lookup_define_hidden_class_kill_defines_first() {
        assert_kill_row("lk_define_hidden_class_full", &hidden_full_row(true));
    }

    fn class_data_row(kill: bool) -> crate::test_utils::SpentDefineRow {
        let mut ctx = MockNativeContext::new();
        let (lookup, _) = make_lookup_for(&mut ctx, HOST);
        let arr = bytes_array(&mut ctx);
        let payload = ctx.alloc_object(ClassId::new(1), 1);
        let opts = no_options(&mut ctx);
        spent_define_row(&mut ctx, kill, |ctx| {
            lk_define_hidden_class_with_class_data(
                ctx,
                &[
                    Value::Object(Some(lookup)),
                    Value::Object(Some(arr)),
                    Value::Object(Some(payload)),
                    Value::Int(0),
                    Value::Object(Some(opts)),
                ],
            )
        })
    }

    #[test]
    fn zs10nd9_lookup_define_hidden_class_with_class_data_fails_before_it_publishes() {
        assert_reserved_row(
            "lk_define_hidden_class_with_class_data",
            &class_data_row(false),
        );
    }

    #[test]
    fn zs10nd9_lookup_define_hidden_class_with_class_data_kill_defines_first() {
        assert_kill_row(
            "lk_define_hidden_class_with_class_data",
            &class_data_row(true),
        );
    }
}

#[test]
fn registration_follows_the_jdk_body_policy() {
    for word in [None, Some("0"), Some("1")] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_LOOKUP_DEFINE_JDK_BODY", word)],
            || {
                let mut r = NativeMethodRegistry::new();
                register_lookup_define_class(&mut r);
                for (method, descriptor) in [
                    ("defineClass", "([B)Ljava/lang/Class;"),
                    (
                        "defineHiddenClass",
                        "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;",
                    ),
                    (
                        "defineHiddenClassWithClassData",
                        "([BLjava/lang/Object;Z[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;",
                    ),
                ] {
                    assert!(
                        r.find(LK_CLASS, method, descriptor).is_none(),
                        "{method} policy={word:?}"
                    );
                }
                assert!(
                    r.find(
                        LK_CLASS,
                        "findClass",
                        "(Ljava/lang/String;)Ljava/lang/Class;"
                    )
                    .is_some()
                );
            },
        );
    }
}
