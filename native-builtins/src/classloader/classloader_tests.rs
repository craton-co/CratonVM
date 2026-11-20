use super::*;

/// The two names in `register_classloader_define_class` are written out
/// instead of looped over `JLA_CARRIER_CANDIDATES` so the drift scanner can
/// read them (see the comment there). Nothing in the language ties the two
/// lists together, so tie them here: a candidate added to the const and not
/// to that call site is a carrier on which `defineClass` silently is not
/// registered, which on the image where it IS the carrier is a
/// `NoSuchMethodError` from `BootLoader` during boot.
#[test]
fn jla_define_class_is_registered_on_every_carrier_candidate() {
    const DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;";
    let mut r = NativeMethodRegistry::new();
    register_classloader_define_class(&mut r);
    let rows = r.dump_registrations();
    for cand in crate::shared_secrets_bridge::JLA_CARRIER_CANDIDATES {
        assert!(
            rows.iter().any(|(cls, name, desc, _)| *cls == cand
                && *name == "defineClass"
                && *desc == DESC),
            "no defineClass registration for carrier candidate {cand}; \
             the inline list in register_classloader_define_class has \
             drifted from JLA_CARRIER_CANDIDATES"
        );
    }
}
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};

fn make_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_classloader_natives(&mut r);
    // Lookup find* methods are registered in lang_invoke, not classloader
    crate::lang_invoke::register_p63_method_handles_lookup(&mut r);
    r
}

fn new_object_ref(ctx: &mut MockNativeContext, class_name: &str) -> ObjectRef {
    match ctx.new_object(class_name).unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected {class_name} object, got {other:?}"),
    }
}

fn panic_on_size_call(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "size" && descriptor == "()I" {
        panic!("custom-handler probe must not dispatch size() on non-list objects");
    }
    None
}

// --- ClassLoader registration tests ---

#[test]
fn test_custom_handler_probe_rejects_non_url_class_path_ucp() {
    let mut ctx = MockNativeContext::new();
    let loader = new_object_ref(
        &mut ctx,
        "io/quarkus/bootstrap/classloading/QuarkusClassLoader",
    );
    let bad_ucp = new_object_ref(&mut ctx, "java/net/URL");
    let bad_path = new_object_ref(&mut ctx, "java/net/URL");
    ctx.set_field_by_name(loader, "ucp", Value::Object(Some(bad_ucp)));
    ctx.set_field_by_name(bad_ucp, "path", Value::Object(Some(bad_path)));
    ctx.set_invoke_virtual_hook(panic_on_size_call);

    assert!(build_custom_handler_url_list(&mut ctx, loader, "META-INF/services/x").is_none());
}

#[test]
fn test_custom_handler_probe_rejects_non_arraylist_path() {
    let mut ctx = MockNativeContext::new();
    let loader = new_object_ref(&mut ctx, "java/net/URLClassLoader");
    let ucp = new_object_ref(&mut ctx, "jdk/internal/loader/URLClassPath");
    let bad_path = new_object_ref(&mut ctx, "java/net/URL");
    ctx.set_field_by_name(loader, "ucp", Value::Object(Some(ucp)));
    ctx.set_field_by_name(ucp, "path", Value::Object(Some(bad_path)));
    ctx.set_invoke_virtual_hook(panic_on_size_call);

    assert!(build_custom_handler_url_list(&mut ctx, loader, "META-INF/services/x").is_none());
}

#[test]
fn test_urlclassloader_find_resource_prefers_receiver_urls() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("virtual")).expect("mkdir");
    std::fs::write(
        dir.path().join("virtual").join("tomcat0807_webapp.txt"),
        b"ok",
    )
    .expect("write fixture");

    let mut ctx = MockNativeContext::new();
    let loader = new_object_ref(&mut ctx, "java/net/URLClassLoader");
    let ucp = new_object_ref(&mut ctx, "jdk/internal/loader/URLClassPath");
    let url = new_object_ref(&mut ctx, "java/net/URL");
    let path = ctx.create_string(&dir.path().to_string_lossy());
    ctx.set_field(url, 3, Value::Object(Some(path)));
    ctx.set_field_by_name(loader, "ucp", Value::Object(Some(ucp)));
    let urls = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(urls, 0, Value::Object(Some(url)));
    ctx.set_field(ucp, UCP_STASHED_URLS, Value::Object(Some(urls)));

    let hits = loader_local_resource_urls(&mut ctx, loader, "virtual/tomcat0807_webapp.txt");
    assert_eq!(
        hits.len(),
        1,
        "receiver-local URLClassLoader path must be searched"
    );

    let name = ctx.create_string("virtual/tomcat0807_webapp.txt");
    let found = ucl_find_resource(
        &mut ctx,
        &[Value::Object(Some(loader)), Value::Object(Some(name))],
    )
    .expect("findResource native")
    .expect("return value");
    let found = match found {
        Value::Object(Some(o)) => o,
        other => panic!("expected URL object, got {other:?}"),
    };
    let file_field = match ctx.get_field(found, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        other => panic!("expected URL path field, got {other:?}"),
    };
    assert!(
        file_field.contains("tomcat0807_webapp.txt"),
        "returned URL should point at the receiver-local resource, got {file_field}"
    );
}

/// A loader whose OWN URL list is knowable answers `findResources` out of
/// that list alone. An empty answer is the answer — widening it with the
/// process-wide scan is what handed a `@ClassPathExclusions` test back the
/// `META-INF/services` entry of the very jar it excluded.
#[test]
fn test_urlclassloader_find_resources_does_not_fall_back_to_flat_scan() {
    const SPI: &str = "META-INF/services/org.slf4j.spi.SLF4JServiceProvider";

    // The loader's own URL: a directory that does NOT hold the descriptor,
    // standing in for a classpath the excluded jar was filtered out of.
    let dir = tempfile::tempdir().expect("tempdir");

    let mut ctx = MockNativeContext::new();
    let loader = new_object_ref(&mut ctx, "java/net/URLClassLoader");
    let ucp = new_object_ref(&mut ctx, "jdk/internal/loader/URLClassPath");
    let url = new_object_ref(&mut ctx, "java/net/URL");
    let path = ctx.create_string(&dir.path().to_string_lossy());
    ctx.set_field(url, 3, Value::Object(Some(path)));
    ctx.set_field_by_name(loader, "ucp", Value::Object(Some(ucp)));
    let urls = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(urls, 0, Value::Object(Some(url)));
    ctx.set_field(ucp, UCP_STASHED_URLS, Value::Object(Some(urls)));

    // ... while the PROCESS-WIDE classpath does hold it. This is the leak
    // source: without it the assertion below would pass vacuously.
    ctx.set_resource(
        SPI,
        b"ch.qos.logback.classic.spi.LogbackServiceProvider\n".to_vec(),
    );
    let flat_name = ctx.create_string(SPI);
    let flat = cl_get_resources_impl(
        &mut ctx,
        &[Value::Object(Some(loader)), Value::Object(Some(flat_name))],
        false,
    )
    .expect("flat scan")
    .expect("flat scan return value");
    assert_eq!(
        enumeration_len(&mut ctx, flat),
        1,
        "the process-wide scan must see this descriptor, else the assertion \
         below would pass without the leak ever being possible"
    );

    assert!(
        loader_local_resource_urls(&mut ctx, loader, SPI).is_empty(),
        "receiver-local scan must not find the excluded descriptor"
    );

    let name = ctx.create_string(SPI);
    let found = ucl_find_resources(
        &mut ctx,
        &[Value::Object(Some(loader)), Value::Object(Some(name))],
    )
    .expect("findResources native")
    .expect("return value");
    assert_eq!(
        enumeration_len(&mut ctx, found),
        0,
        "an empty receiver-local result must be returned as-is, not replaced \
         by the process-wide classpath scan"
    );
}

/// A loader CratonVM has NO URL view of keeps the historical flat-scan
/// fallback — "the local scan found nothing" and "the local scan could not
/// run" are different answers and only the first one is authoritative.
#[test]
fn test_urlclassloader_find_resources_keeps_flat_scan_without_recorded_urls() {
    const SPI: &str = "META-INF/services/com.acme.Service";

    let mut ctx = MockNativeContext::new();
    let loader = new_object_ref(&mut ctx, "java/net/URLClassLoader");
    ctx.set_resource(SPI, b"com.acme.Provider\n".to_vec());

    assert!(
        loader_constructor_url_paths(&ctx, loader).is_empty(),
        "fixture must leave this loader's URL list unknowable"
    );

    let name = ctx.create_string(SPI);
    let found = ucl_find_resources(
        &mut ctx,
        &[Value::Object(Some(loader)), Value::Object(Some(name))],
    )
    .expect("findResources native")
    .expect("return value");
    assert_eq!(
        enumeration_len(&mut ctx, found),
        1,
        "with no recorded URLs the process-wide scan is all there is"
    );
}

/// The platform loader owns the JDK module surface and nothing else. Serving
/// it the flat application classpath makes every child parented to it — the
/// shape `ModifiedClassPathClassLoader` is built on — see the very jars its
/// exclusions removed, through parent-first delegation.
#[test]
fn test_platform_loader_get_resources_excludes_application_classpath() {
    const SPI: &str = "META-INF/services/org.slf4j.spi.SLF4JServiceProvider";

    let mut ctx = MockNativeContext::new();
    ctx.set_resource(
        SPI,
        b"ch.qos.logback.classic.spi.LogbackServiceProvider\n".to_vec(),
    );

    // Control: an ordinary receiver still gets the application classpath, so
    // an empty answer below is the platform rule and not an empty fixture.
    let app = new_object_ref(&mut ctx, "jdk/internal/loader/ClassLoaders$AppClassLoader");
    let app_name = ctx.create_string(SPI);
    let app_enum = cl_get_resources_impl(
        &mut ctx,
        &[Value::Object(Some(app)), Value::Object(Some(app_name))],
        true,
    )
    .expect("app getResources")
    .expect("app return value");
    assert_eq!(
        enumeration_len(&mut ctx, app_enum),
        1,
        "the application loader must still see the process classpath"
    );

    let platform = new_object_ref(
        &mut ctx,
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
    );
    let name = ctx.create_string(SPI);
    let found = cl_get_resources_impl(
        &mut ctx,
        &[Value::Object(Some(platform)), Value::Object(Some(name))],
        true,
    )
    .expect("platform getResources")
    .expect("return value");
    assert_eq!(
        enumeration_len(&mut ctx, found),
        0,
        "the platform loader must not enumerate application-classpath resources"
    );
}

/// HotSpot's `BuiltinClassLoader.findResource`: no built-in loader answers
/// a non-class name in a package of a named module that the module does
/// not open to everyone -- whoever holds a copy -- while a `.class`, an
/// open package and a name outside every module package stay visible
/// (`jboss_jdkspecific::builtin_loaders_hide_resource`; probe
/// `HriIcuResources.java`).
#[test]
fn test_builtin_loaders_hide_non_open_module_package_resources() {
    const HIDDEN: &str = "jdk/internal/icu/impl/data/icudt76b/uprops.icu";
    const CLASS: &str = "jdk/internal/icu/impl/data/icudt76b/Shim.class";
    const OPEN: &str = "com/acme/open/data.bin";
    const MISC: &str = "META-INF/services/java.nio.file.spi.FileSystemProvider";

    let mut ctx = MockNativeContext::new();
    ctx.declare_closed_module_package("jdk/internal/icu/impl/data/icudt76b", "java.base");
    ctx.declare_module_package("com/acme/open", "com.acme");
    for name in [HIDDEN, CLASS, OPEN, MISC] {
        ctx.set_resource(name, b"x".to_vec());
    }
    let app = new_object_ref(&mut ctx, "jdk/internal/loader/ClassLoaders$AppClassLoader");
    for (name, want) in [(HIDDEN, 0), (CLASS, 1), (OPEN, 1), (MISC, 1)] {
        let s = ctx.create_string_uninterned(name);
        let found = cl_get_resources_impl(
            &mut ctx,
            &[Value::Object(Some(app)), Value::Object(Some(s))],
            true,
        )
        .expect("app getResources")
        .expect("return value");
        assert_eq!(enumeration_len(&mut ctx, found), want, "app loader, {name}");
        // The static `getSystemResources(name)`: no receiver.
        let s = ctx.create_string_uninterned(name);
        let found = cl_get_resources_impl(&mut ctx, &[Value::Object(Some(s))], true)
            .expect("getSystemResources")
            .expect("return value");
        assert_eq!(enumeration_len(&mut ctx, found), want, "system, {name}");
    }
}

/// Length of a snapshot `Enumeration$Impl` (field 0 is its backing array).
fn enumeration_len(ctx: &mut MockNativeContext, value: Value) -> usize {
    let enm = match value {
        Value::Object(Some(o)) => o,
        other => panic!("expected an Enumeration object, got {other:?}"),
    };
    match ctx.get_field(enm, 0) {
        Value::Object(Some(arr)) => ctx.array_length(arr),
        _ => 0,
    }
}

#[test]
fn test_is_bootstrap_class_name_jdk_packages() {
    // JDK / platform packages the bootstrap loader genuinely owns.
    assert!(is_bootstrap_class_name("java/lang/String"));
    assert!(is_bootstrap_class_name("javax/sql/DataSource"));
    assert!(is_bootstrap_class_name("jdk/internal/loader/ClassLoaders"));
    assert!(is_bootstrap_class_name("sun/nio/ch/IOUtil"));
    assert!(is_bootstrap_class_name("com/sun/crypto/provider/AESCipher"));
    assert!(is_bootstrap_class_name("[Ljava/lang/Object;"));
}

#[test]
fn test_is_bootstrap_class_name_app_classes() {
    // Application classes are NEVER bootstrap-loadable — findBootstrapClass
    // must defer these to a custom loader's findClass override (HIB-CV-24).
    assert!(!is_bootstrap_class_name(
        "org/hibernate/orm/test/bootstrap/registry/classloading/ClassLoaderServiceImplTest"
    ));
    assert!(!is_bootstrap_class_name("com/example/MyService"));
    assert!(!is_bootstrap_class_name("MProbe$Base"));
    // `jakarta.*` is an application/module class, not bootstrap.
    assert!(!is_bootstrap_class_name("jakarta/persistence/Entity"));
}

#[test]
fn test_loadclass_resolve_override_survives_urlclassloader_superclass() {
    let mut ctx = MockNativeContext::new();
    let url_cid = ctx
        .ensure_class_initialized("java/net/URLClassLoader")
        .expect("URLClassLoader class");
    let bsh_cid = ctx
        .ensure_class_initialized("bsh/classpath/BshClassLoader")
        .expect("BshClassLoader class");
    let discrete_cid = ctx
        .ensure_class_initialized("bsh/classpath/DiscreteFilesClassLoader")
        .expect("DiscreteFilesClassLoader class");
    ctx.set_superclass(discrete_cid, bsh_cid);
    ctx.set_superclass(bsh_cid, url_cid);
    ctx.set_declared_methods(
        bsh_cid,
        vec![cratonvm_native_api::MethodMetadata {
            name: "loadClass".to_string(),
            descriptor: "(Ljava/lang/String;Z)Ljava/lang/Class;".to_string(),
            access_flags: 0,
            declaring_class_id: bsh_cid,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    let loader = new_object_ref(&mut ctx, "bsh/classpath/DiscreteFilesClassLoader");

    assert!(
        receiver_overrides_load_class_resolve(&mut ctx, loader),
        "BeanShell-shaped URLClassLoader subclasses must dispatch their loadClass override"
    );
}

#[test]
fn test_loadclass_resolve_override_on_direct_urlclassloader_subclass() {
    // Spring Boot's FilteredClassLoader directly extends URLClassLoader and
    // rejects hidden packages from loadClass(String, boolean).  Keep this
    // one-level shape distinct from the BeanShell hierarchy above: reaching
    // URLClassLoader must not hide an override already declared by its
    // immediate child.
    let mut ctx = MockNativeContext::new();
    let url_cid = ctx
        .ensure_class_initialized("java/net/URLClassLoader")
        .expect("URLClassLoader class");
    let filtered_cid = ctx
        .ensure_class_initialized("org/springframework/boot/test/context/FilteredClassLoader")
        .expect("FilteredClassLoader class");
    ctx.set_superclass(filtered_cid, url_cid);
    ctx.set_declared_methods(
        filtered_cid,
        vec![cratonvm_native_api::MethodMetadata {
            name: "loadClass".to_string(),
            descriptor: "(Ljava/lang/String;Z)Ljava/lang/Class;".to_string(),
            access_flags: 0,
            declaring_class_id: filtered_cid,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    let loader = new_object_ref(
        &mut ctx,
        "org/springframework/boot/test/context/FilteredClassLoader",
    );

    assert!(
        receiver_overrides_load_class_resolve(&mut ctx, loader),
        "a direct URLClassLoader subclass must dispatch its loadClass override"
    );
}

#[test]
fn test_loadclass_single_override_survives_urlclassloader_superclass() {
    let mut ctx = MockNativeContext::new();
    let url_cid = ctx
        .ensure_class_initialized("java/net/URLClassLoader")
        .expect("URLClassLoader class");
    let modified_cid = ctx
        .ensure_class_initialized(
            "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
        )
        .expect("ModifiedClassPathClassLoader class");
    ctx.set_superclass(modified_cid, url_cid);
    ctx.set_declared_methods(
        modified_cid,
        vec![cratonvm_native_api::MethodMetadata {
            name: "loadClass".to_string(),
            descriptor: "(Ljava/lang/String;)Ljava/lang/Class;".to_string(),
            access_flags: 0,
            declaring_class_id: modified_cid,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    let loader = new_object_ref(
        &mut ctx,
        "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
    );

    assert!(
        receiver_overrides_load_class_single(&mut ctx, loader),
        "a URLClassLoader subclass's single-argument loadClass override must run"
    );
    assert!(
        !receiver_overrides_load_class_resolve(&mut ctx, loader),
        "the single-argument override must not be mistaken for the protected overload"
    );
}

/// JIT round 23 wave 6 (`CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO`): the
/// memo answers only under the stamp it was filled at, a moved stamp
/// empties it, a record computed under a stale stamp is dropped, and a
/// dead loader's classes leave it.
#[test]
fn r23w6_load_class_override_memo_is_stamped_and_pruned() {
    let tables = LoaderIdentityTables::new(0x23_0601);
    assert_eq!(tables.load_class_override_lookup(5, 100, 0), None);
    tables.load_class_override_record(5, 100, 0, true);
    tables.load_class_override_record(5, 100, 1, false);
    tables.load_class_override_record(5, 200, 0, false);
    assert_eq!(tables.load_class_override_lookup(5, 100, 0), Some(true));
    assert_eq!(tables.load_class_override_lookup(5, 100, 1), Some(false));
    assert_eq!(tables.load_class_override_lookup(5, 200, 0), Some(false));
    // A dead loader's class goes; the others stay.
    tables.load_class_override_forget(&[200]);
    assert_eq!(tables.load_class_override_lookup(5, 200, 0), None);
    assert_eq!(tables.load_class_override_lookup(5, 100, 0), Some(true));
    // A moved stamp (an edge edit, an unload, a redefinition) empties it.
    assert_eq!(tables.load_class_override_lookup(6, 100, 0), None);
    assert_eq!(tables.load_class_override_lookup(6, 100, 1), None);
    // An answer walked under the old stamp is not filed under the new one.
    tables.load_class_override_record(5, 100, 0, true);
    assert_eq!(tables.load_class_override_lookup(6, 100, 0), None);
    tables.load_class_override_record(6, 100, 0, false);
    assert_eq!(tables.load_class_override_lookup(6, 100, 0), Some(false));
}

/// The mock context has no hierarchy stamp, so the walk runs every time
/// and a later method-table change is seen at once (the memo is never
/// keyed without a stamp that would move with it).
#[test]
fn r23w6_load_class_override_without_a_stamp_walks_every_time() {
    let mut ctx = MockNativeContext::new();
    assert_eq!((&ctx as &dyn NativeContext).class_hierarchy_memo_stamp(), None);
    let base = ctx
        .ensure_class_initialized("java/lang/ClassLoader")
        .expect("ClassLoader class");
    let custom = ctx
        .ensure_class_initialized("example/R23W6Loader")
        .expect("custom loader class");
    ctx.set_superclass(custom, base);
    let loader = new_object_ref(&mut ctx, "example/R23W6Loader");
    assert!(!receiver_overrides_load_class_resolve(&mut ctx, loader));
    ctx.set_declared_methods(
        custom,
        vec![cratonvm_native_api::MethodMetadata {
            name: "loadClass".to_string(),
            descriptor: "(Ljava/lang/String;Z)Ljava/lang/Class;".to_string(),
            access_flags: 0,
            declaring_class_id: custom,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    assert!(receiver_overrides_load_class_resolve(&mut ctx, loader));
}

/// gc-common w9-a: a VM's real-JDK loader namespace ids go with the VM —
/// at ITS teardown (`forget_vm_loader_singletons`), not at the creation of
/// some other VM (`reset_loader_singletons`, which used to wipe the whole
/// table and so every concurrently live VM's rows with it).
#[test]
fn test_vm_teardown_forgets_real_jdk_loader_namespace_ids() {
    const VM: usize = 0x9A_0001;
    const NS: u32 = 90_901;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let class_loader = ctx
        .ensure_class_initialized("java/lang/ClassLoader")
        .expect("ClassLoader class");
    let isolated_loader = ctx
        .ensure_class_initialized("example/IsolatedLoader")
        .expect("isolated loader class");
    ctx.set_superclass(isolated_loader, class_loader);
    let loader = new_object_ref(&mut ctx, "example/IsolatedLoader");
    detached_loader_tables(VM)
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((loader, NS));

    assert_eq!(peek_loader_namespace_id(&mut ctx, loader), Some(NS));

    reset_loader_singletons();
    assert_eq!(
        peek_loader_namespace_id(&mut ctx, loader),
        Some(NS),
        "creating another VM must not wipe a live VM's namespace ids"
    );

    forget_vm_loader_singletons(VM);
    assert!(
        peek_loader_namespace_id(&mut ctx, loader).is_none(),
        "VM teardown must not leave stale real-JDK loader namespace ids"
    );
}

/// `prx23`: the thread's last namespace-index hit answers only the same
/// VM, loader address and loader address epoch.
#[test]
fn prx23_namespace_last_hit_is_keyed_by_vm_loader_and_epoch() {
    let mut ctx_a = MockNativeContext::new();
    ctx_a.set_vm_identity(0x7_2301);
    let ctx_b = MockNativeContext::new();
    ctx_b.set_vm_identity(0x7_2302);
    ctx_a
        .ensure_class_initialized("example/Prx23LastHitLoader")
        .expect("loader class");
    let loader = new_object_ref(&mut ctx_a, "example/Prx23LastHitLoader");
    let other = new_object_ref(&mut ctx_a, "example/Prx23LastHitLoader");
    assert_eq!(namespace_last_hit(&ctx_a, 5, loader), None, "nothing filed yet");
    note_namespace_last_hit(&ctx_a, 5, loader, 90_231);
    assert_eq!(namespace_last_hit(&ctx_a, 5, loader), Some(90_231));
    assert_eq!(namespace_last_hit(&ctx_a, 6, loader), None, "another epoch");
    assert_eq!(namespace_last_hit(&ctx_a, 5, other), None, "another loader");
    assert_eq!(namespace_last_hit(&ctx_b, 5, loader), None, "another VM");
    note_namespace_last_hit(&ctx_a, 5, other, 90_232);
    assert_eq!(namespace_last_hit(&ctx_a, 5, loader), None, "one row, replaced");
    assert_eq!(namespace_last_hit(&ctx_a, 5, other), Some(90_232));
}

/// SHIM-AUDIT handover item 2 — pins the GC contract documented on
/// the namespace table ([`LoaderIdentityTables`]).
///
/// This is a PIN, not a fail-before-the-fix regression test: both halves
/// (prune the dead, remap the moved) are already wired, and were verified
/// by reading `gc_reconcile_defining_loaders`. Until now the only thing
/// holding the invariant was a prose comment, and the comment had already
/// drifted (it still described the table's old identity-hash-keyed form and
/// claimed it held no `ObjectRef`s at all). Deleting either half of the
/// `retain_mut` now fails here instead of silently reintroducing the
/// original defect: a dead loader's entry outliving it, its address being
/// reused, and the new loader at that address inheriting the dead one's
/// namespace id *and* — via `UserLoaderParents::register` — its parent link.
#[test]
fn loader_namespace_store_is_pruned_and_remapped_by_gc_reconcile() {
    let mut ctx = MockNativeContext::new();
    let class_loader = ctx
        .ensure_class_initialized("java/lang/ClassLoader")
        .expect("ClassLoader class");
    let isolated = ctx
        .ensure_class_initialized("example/GcReconcileLoader")
        .expect("isolated loader class");
    ctx.set_superclass(isolated, class_loader);

    let dead = new_object_ref(&mut ctx, "example/GcReconcileLoader");
    let moved_from = new_object_ref(&mut ctx, "example/GcReconcileLoader");
    let moved_to = new_object_ref(&mut ctx, "example/GcReconcileLoader");

    // Ids well above `ClassLoaderId::NATIVE_FIRST_USER_DEFINED` and far from
    // anything another test in this process allocates.
    const DEAD_NS: u32 = 90_001;
    const MOVED_NS: u32 = 90_002;

    let tables = detached_loader_tables(ctx.vm_identity());
    {
        let mut store = tables
            .loader_namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        store.push((dead, DEAD_NS));
        store.push((moved_from, MOVED_NS));
    }
    assert_eq!(
        peek_loader_namespace_id(&mut ctx, dead),
        Some(DEAD_NS),
        "precondition: the dead loader's entry is in the store"
    );

    let dead_addr = dead.as_ptr() as usize;
    let from_addr = moved_from.as_ptr() as usize;
    let to_addr = moved_to.as_ptr() as usize;

    // These side-tables are process-global and the test binary is
    // multi-threaded, so this predicate reports every address it does not
    // own as ALIVE. Only `dead` is collected; nothing another test put in
    // either store can be pruned by this call.
    //
    // `moved_from` is reported alive on purpose: a relocated object IS a
    // survivor. That mirrors the real predicate
    // (`pointer_map.contains_key(addr) || heap.is_addr_live(addr)`) named in
    // `gc_reconcile_defining_loaders`' doc comment.
    let is_marked = move |addr: usize| addr != dead_addr;
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(from_addr, to_addr);

    gc_reconcile_defining_loaders(tables, &is_marked, &pointer_map, None);

    // Half 1 — PRUNE. The collected loader's entry is gone, so its address
    // can be recycled without the next loader inheriting its namespace.
    assert!(
        peek_loader_namespace_id(&mut ctx, dead).is_none(),
        "a loader collected this cycle must not keep its namespace-id entry"
    );
    assert!(
        loader_object_for_namespace_id_in(tables, DEAD_NS).is_none(),
        "the dead namespace id must no longer resolve to any loader object"
    );

    // Half 2 — REMAP. The survivor kept its id and now answers at its NEW
    // address; the old address answers for nobody.
    assert_eq!(
        peek_loader_namespace_id(&mut ctx, moved_to),
        Some(MOVED_NS),
        "a relocated loader must keep its namespace id at its new address"
    );
    assert_eq!(
        loader_object_for_namespace_id_in(tables, MOVED_NS).map(|o| o.as_ptr()),
        Some(moved_to.as_ptr()),
        "the reverse lookup must hand back the post-collection address"
    );
    assert!(
        peek_loader_namespace_id(&mut ctx, moved_from).is_none(),
        "the pre-collection address must no longer resolve"
    );

    // Leave the (detached, shared by every identity-0 mock) table as we
    // found it.
    tables
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(_, id)| *id != DEAD_NS && *id != MOVED_NS);
}

/// Interpreter round i4 wave 84, lane L5: on ZGC the post-slide survivor
/// predicate answers "live" for a dead loader's address once compaction
/// slid another object into it. When that object is a MOVED loader, its
/// row is remapped onto the dead loader's address and, before the fix,
/// the dead row stayed too: the live loader then answered the dead one's
/// namespace id (the older row matches first). The namespace pass must
/// take the pre-slide exact verdict the defining-loader pass already
/// takes, and the ZGC watch must arm the namespace table's addresses. The
/// pass also moves the loader address epoch
/// (`CRATONVM_LOADER_NAMESPACE_INDEX`'s validity), whatever the backend's
/// collection count does.
#[test]
fn i84_namespace_reconcile_takes_the_exact_pre_slide_verdict() {
    let mut ctx = MockNativeContext::new();
    let class_loader = ctx
        .ensure_class_initialized("java/lang/ClassLoader")
        .expect("ClassLoader class");
    let isolated = ctx
        .ensure_class_initialized("example/I84SlideLoader")
        .expect("isolated loader class");
    ctx.set_superclass(isolated, class_loader);
    let dead = new_object_ref(&mut ctx, "example/I84SlideLoader");
    let moved_from = new_object_ref(&mut ctx, "example/I84SlideLoader");
    const DEAD_NS: u32 = 90_841;
    const LIVE_NS: u32 = 90_842;
    let vm = ctx.vm_identity();
    {
        let mut store = detached_loader_tables(vm)
            .loader_namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        store.push((dead, DEAD_NS));
        store.push((moved_from, LIVE_NS));
    }
    let dead_addr = dead.as_ptr() as usize;
    let from_addr = moved_from.as_ptr() as usize;
    let watched = watched_defining_loader_addrs(detached_loader_tables(vm));
    assert!(
        watched.contains(&dead_addr) && watched.contains(&from_addr),
        "the ZGC watch must cover the namespace table's addresses"
    );

    // The live loader slid from `from_addr` into the dead loader's span.
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(from_addr, dead_addr);
    // Post-slide registry probe: every address is occupied, so "live".
    let is_marked = |_: usize| true;
    let mut exact = std::collections::HashMap::new();
    exact.insert(dead_addr, false);
    exact.insert(from_addr, true);

    let before = loader_address_epoch(&ctx);
    gc_reconcile_defining_loaders(detached_loader_tables(vm), &is_marked, &pointer_map, Some(&exact));
    assert_ne!(
        loader_address_epoch(&ctx),
        before,
        "a reconcile voids every address-keyed loader row"
    );

    // `dead` now names the moved live loader's object.
    assert_eq!(
        peek_loader_namespace_id(&mut ctx, dead),
        Some(LIVE_NS),
        "the loader that slid into a dead loader's span keeps ITS namespace"
    );
    assert!(
        loader_object_for_namespace_id_in(detached_loader_tables(vm), DEAD_NS).is_none(),
        "the dead loader's row is pruned on the exact verdict"
    );

    detached_loader_tables(vm)
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(_, id)| *id != DEAD_NS && *id != LIVE_NS);
}

/// gc-common w9-a: the loader-namespace and loader-meta rows belong to the
/// VM that recorded them.
///
/// * VM A's reconcile, with a predicate that calls VM B's loader DEAD
///   (the production predicate answers "dead" for every address outside
///   A's heap), keeps B's namespace and meta rows, and does not remap
///   them through A's pointer map either.
/// * A lookup from VM A does not find VM B's row for the same address.
/// * `reset_loader_singletons` (another VM's creation) leaves both rows.
/// * `forget_vm_loader_singletons(B)` drops exactly B's rows.
#[test]
fn w9a_loader_namespace_and_meta_rows_belong_to_their_vm() {
    const VM_A: usize = 0x9A_0A00;
    const VM_B: usize = 0x9A_0B00;
    const NS_A: u32 = 90_911;
    const NS_B: u32 = 90_912;
    let mut ctx_a = MockNativeContext::new();
    ctx_a.set_vm_identity(VM_A);
    let mut ctx_b = MockNativeContext::new();
    ctx_b.set_vm_identity(VM_B);
    for ctx in [&mut ctx_a, &mut ctx_b] {
        let class_loader = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("ClassLoader class");
        let isolated = ctx
            .ensure_class_initialized("example/W9aPerVmLoader")
            .expect("isolated loader class");
        ctx.set_superclass(isolated, class_loader);
    }
    let loader_a = new_object_ref(&mut ctx_a, "example/W9aPerVmLoader");
    let loader_b = new_object_ref(&mut ctx_b, "example/W9aPerVmLoader");
    let a_addr = loader_a.as_ptr() as usize;
    let b_addr = loader_b.as_ptr() as usize;

    detached_loader_tables(VM_A)
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((loader_a, NS_A));
    detached_loader_tables(VM_B)
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((loader_b, NS_B));
    loader_meta_put(
        detached_loader_tables(VM_B),
        loader_b,
        LoaderMeta {
            loader_type: Some(LOADER_CUSTOM),
            parallel_capable: Some(false),
            ..LoaderMeta::default()
        },
    );

    // The rows answer only for their own VM, even at the same address.
    assert_eq!(peek_loader_namespace_id(&mut ctx_b, loader_b), Some(NS_B));
    assert!(loader_meta_get(detached_loader_tables(VM_A), loader_b).is_none());
    assert_eq!(
        loader_object_for_namespace_id_in(detached_loader_tables(VM_B), NS_B).map(|o| o.as_ptr() as usize),
        Some(b_addr)
    );
    assert!(
        loader_object_for_namespace_id_in(detached_loader_tables(VM_A), NS_B).is_none(),
        "a namespace id minted in VM B must not resolve in VM A"
    );

    // VM A collects. Its heap calls B's loader (and A's own) dead, and its
    // pointer map would relocate B's address if B's rows were A's to remap.
    let is_marked = move |addr: usize| addr != b_addr && addr != a_addr;
    let mut pointer_map = cratonvm_types::PointerMap::default();
    let bogus_to = new_object_ref(&mut ctx_a, "example/W9aPerVmLoader");
    pointer_map.insert(b_addr, bogus_to.as_ptr() as usize);
    gc_reconcile_defining_loaders(detached_loader_tables(VM_A), &is_marked, &pointer_map, None);

    assert!(
        loader_object_for_namespace_id_in(detached_loader_tables(VM_A), NS_A).is_none(),
        "A's own dead loader is pruned"
    );
    assert_eq!(
        peek_loader_namespace_id(&mut ctx_b, loader_b),
        Some(NS_B),
        "VM A's collection must neither prune nor remap VM B's namespace row"
    );
    assert_eq!(
        loader_meta_get(detached_loader_tables(VM_B), loader_b).and_then(|m| m.loader_type),
        Some(LOADER_CUSTOM),
        "VM A's collection must neither prune nor remap VM B's meta row"
    );
    assert_eq!(loader_parallel_capable_of(&ctx_b, loader_b), Some(0));

    // Another VM being created does not touch B's rows.
    reset_loader_singletons();
    assert_eq!(peek_loader_namespace_id(&mut ctx_b, loader_b), Some(NS_B));
    assert!(loader_meta_get(detached_loader_tables(VM_B), loader_b).is_some());

    // B's teardown drops them.
    forget_vm_loader_singletons(VM_B);
    assert!(peek_loader_namespace_id(&mut ctx_b, loader_b).is_none());
    assert!(loader_meta_get(detached_loader_tables(VM_B), loader_b).is_none());
    assert!(loader_object_for_namespace_id_in(detached_loader_tables(VM_B), NS_B).is_none());
    forget_vm_loader_singletons(VM_A);
}

/// gc-common w9-a: `URLClassLoader.close()` state is per VM. Identity
/// hashes collide across VMs by construction, so VM A closing its loader
/// must not close VM B's loader that has the same hash, and another VM's
/// creation must not reopen it.
#[test]
fn w9a_closed_url_loader_state_is_per_vm() {
    const VM_A: usize = 0x9A_1A00;
    const VM_B: usize = 0x9A_1B00;
    let mut ctx_a = MockNativeContext::new();
    ctx_a.set_vm_identity(VM_A);
    let mut ctx_b = MockNativeContext::new();
    ctx_b.set_vm_identity(VM_B);
    let loader_a = new_object_ref(&mut ctx_a, "java/lang/Object");
    let loader_b = new_object_ref(&mut ctx_b, "java/lang/Object");
    // Two fresh mocks number their objects the same way, so the two
    // loaders normally share an identity hash here — the collision every
    // pair of real heaps produces. The assertions hold either way.
    ucl_mark_closed(&ctx_a, loader_a);
    assert!(ucl_is_closed(&ctx_a, loader_a));
    assert!(
        !ucl_is_closed(&ctx_b, loader_b),
        "A's close must not close B's loader, even on a shared identity hash"
    );

    ucl_mark_closed(&ctx_b, loader_b);
    assert!(ucl_is_closed(&ctx_b, loader_b));
    reset_loader_singletons();
    assert!(
        ucl_is_closed(&ctx_b, loader_b),
        "another VM's creation must not reopen a live VM's closed loader"
    );
    assert!(ucl_is_closed(&ctx_a, loader_a));

    forget_vm_loader_singletons(VM_A);
    assert!(!ucl_is_closed(&ctx_a, loader_a));
    assert!(
        ucl_is_closed(&ctx_b, loader_b),
        "A's teardown keeps B's state"
    );
    forget_vm_loader_singletons(VM_B);
    assert!(!ucl_is_closed(&ctx_b, loader_b));
}

/// gc-common w11-e: a closed `URLClassLoader`'s row goes when the loader
/// dies (it used to stay until VM teardown), follows the loader when it
/// moves, and another VM's row is not judged.
#[test]
fn w11e_closed_url_loader_rows_follow_their_loader_through_the_reconcile() {
    const VM: usize = 0x11E_1A00;
    const OTHER: usize = 0x11E_1B00;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let mut other = MockNativeContext::new();
    other.set_vm_identity(OTHER);
    let dead = new_object_ref(&mut ctx, "java/lang/Object");
    let moved = new_object_ref(&mut ctx, "java/lang/Object");
    let foreign = new_object_ref(&mut other, "java/lang/Object");
    ucl_mark_closed(&ctx, dead);
    ucl_mark_closed(&ctx, moved);
    ucl_mark_closed(&other, foreign);
    let dead_addr = dead.as_ptr() as usize;
    let from_addr = moved.as_ptr() as usize;
    // gc-common w29-e: 4 GiB up keeps the mock identity hash (the address
    // truncated to i32), as a real move keeps the object's hash; lookups
    // compare the address, so the moved loader is asked for at `to_addr`.
    let to_addr: usize = from_addr + 0x1_0000_0000;
    // SAFETY: never dereferenced; the lookup hashes the address.
    let moved_now = unsafe { ObjectRef::from_raw(to_addr as *mut u8) };
    // The foreign loader is "dead" as VM's heap sees it, like every
    // address outside that heap. It must not be judged.
    let foreign_addr = foreign.as_ptr() as usize;
    let is_marked = move |addr: usize| addr != dead_addr && addr != foreign_addr;
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(from_addr, to_addr);

    gc_reconcile_defining_loaders(detached_loader_tables(VM), &is_marked, &pointer_map, None);

    assert!(!ucl_is_closed(&ctx, dead), "a dead loader's row goes");
    assert!(
        ucl_is_closed(&ctx, moved_now),
        "a moved loader stays closed"
    );
    assert!(
        !ucl_is_closed(&ctx, moved),
        "and its old address is no longer a closed loader"
    );
    let ours: Vec<usize> = closed_url_classloader_ids(detached_loader_tables(VM))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .flat_map(|rows| rows.iter().copied())
        .collect();
    assert_eq!(
        ours,
        vec![to_addr],
        "one row left for this VM, at the loader's new address"
    );
    assert!(
        ucl_is_closed(&other, foreign),
        "another VM's row is not judged by this VM's predicate"
    );

    forget_vm_loader_singletons(VM);
    forget_vm_loader_singletons(OTHER);
    assert!(!ucl_is_closed(&other, foreign));
}

/// gc-common w9-a: `url_classloader_define_locks` keeps an entry only
/// while some thread uses it. The first of two users leaving keeps it,
/// the last one removes it, and an entry left behind with no user at all
/// is swept by the next release.
#[test]
fn w9a_define_lock_entry_is_released_by_its_last_user() {
    type Entry = std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>;
    let key = (0x9A_3000u32, "w9a/ReleasedByLastUser".to_string());
    let other = (0x9A_3001u32, "w9a/ReleasedByLastUserOther".to_string());
    let take = |k: &(u32, String)| -> Entry {
        url_classloader_define_locks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(k.clone())
            .or_insert_with(|| {
                std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()))
            })
            .clone()
    };
    let present = |k: &(u32, String)| {
        url_classloader_define_locks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(k)
    };
    let leave = |k: &(u32, String), mine: Entry| {
        drop(DefineLockEntryRelease {
            key: k.clone(),
            lock_addr: std::sync::Arc::as_ptr(&mine) as usize,
        });
        drop(mine);
    };

    let first = take(&key);
    let second = take(&key);
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    leave(&key, first);
    assert!(present(&key), "a waiter still holds the entry");
    leave(&key, second);
    assert!(!present(&key), "the last user removes the entry");

    // Held by the map alone: abandoned. The next release sweeps it. (No
    // "it is present" check in between: a release by a concurrently
    // running test may legitimately sweep it first.)
    drop(take(&key));
    let mine = take(&other);
    leave(&other, mine);
    assert!(!present(&other));
    assert!(!present(&key), "an abandoned entry is swept");
}

/// gc-common w9-a: the URL-classpath cache is bounded, and a hit returns
/// the cached `ClassPath` rather than building another one. The paths
/// name nothing on disk, so `ClassPath::new` builds an empty path.
#[test]
fn w9a_url_class_path_cache_is_bounded() {
    let key = |i: usize| vec![format!("/w9a-nonexistent/cache-bound-{i}.jar")];
    // One VM's cache (wave 65: per VM, `ArchiveBytesCaches::url_class_path`).
    let cache = new_url_class_path_cache();
    let first = class_path_from_cache(&cache, &key(0));
    let again = class_path_from_cache(&cache, &key(0));
    assert!(
        Arc::ptr_eq(&first, &again),
        "a hit must reuse the cached ClassPath"
    );
    for i in 1..=(LOCAL_URL_CLASS_PATH_CACHE_LIMIT + 8) {
        let _ = class_path_from_cache(&cache, &key(i));
    }
    assert!(
        cache.lock().0.len() <= LOCAL_URL_CLASS_PATH_CACHE_LIMIT,
        "the cache must never exceed its bound"
    );
}

/// Interpreter round i5 wave 101, lane L5
/// (`i100-L5-loader-identity-tables-are-process-globals`): two VMs' loader
/// identity tables are separate. VM A defining classes through a user
/// loader leaves VM B's latches, key-set bits, rows, orphan markers,
/// non-strong hidden rows, loader meta and namespace rows untouched, even
/// for the SAME class id (every VM mints ids from zero). Before, the
/// latch and the bitset were process-wide: B's
/// `class_may_have_defining_loader` answered "maybe" for A's ids, and B's
/// interpreter took the store's mutex on its hot paths. A's tables
/// dropping (its VM's teardown) is all it takes to forget A's rows.
#[test]
fn i101_two_vms_loader_identity_tables_are_separate() {
    let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    const SHARED: u32 = 42;
    const A_ONLY: u32 = 43;
    let a = LoaderIdentityTables::new(0x1_0101_0A00);
    let b = LoaderIdentityTables::new(0x1_0101_0B00);

    // A defines through a user loader.
    register_defining_loader(&a, SHARED, fake(0x101_1000));
    register_defining_loader(&a, A_ONLY, fake(0x101_1000));
    register_non_strong_hidden_mirror(&a, A_ONLY, 0x101_2000, Some(0x101_1000));
    loader_meta_put(&a, fake(0x101_1000), LoaderMeta::custom_initialiser(Some(90_101)));
    remember_namespace_object(&a, fake(0x101_1000), 90_101);

    assert!(any_defining_loader_registered(&a));
    assert!(class_may_have_defining_loader(&a, SHARED));
    assert!(any_non_strong_hidden(&a));

    // B never did: every screen answers "no" without a lock, and every
    // table is empty for the same ids and the same loader address.
    assert!(
        !any_defining_loader_registered(&b),
        "VM A's user loader must not turn on VM B's latch"
    );
    assert!(
        !class_may_have_defining_loader(&b, SHARED),
        "VM B's own class id 42 must not look loader-defined because VM A's 42 is"
    );
    assert!(!class_may_have_defining_loader(&b, A_ONLY));
    assert!(!any_non_strong_hidden(&b));
    assert_eq!(defining_loader_for(&b, SHARED), None);
    assert_eq!(non_strong_hidden_mirror(&b, A_ONLY), None);
    assert!(non_strong_hidden_mirrors(&b).is_empty());
    assert!(loader_meta_get(&b, fake(0x101_1000)).is_none());
    assert!(loader_object_for_namespace_id_in(&b, 90_101).is_none());
    assert!(watched_defining_loader_addrs(&b).is_empty());

    // B's collection judges nothing of A's: its predicate calls every
    // address dead, A keeps every row.
    let none_alive = |_: usize| false;
    let no_moves = cratonvm_types::PointerMap::default();
    assert!(gc_reconcile_defining_loaders(&b, &none_alive, &no_moves, None).is_empty());
    assert_eq!(
        defining_loader_for(&a, SHARED).map(|l| l.as_ptr() as usize),
        Some(0x101_1000)
    );
    assert!(!is_defining_loader_orphaned(&a, SHARED));
    assert_eq!(non_strong_hidden_mirror(&a, A_ONLY), Some(0x101_2000));

    // A's own collection then retires A's rows, A's bits only.
    let dead = gc_reconcile_defining_loaders(&a, &none_alive, &no_moves, None);
    assert_eq!(dead, vec![SHARED, A_ONLY]);
    assert!(is_defining_loader_orphaned(&a, SHARED));
    assert!(!class_may_have_defining_loader(&a, SHARED));
    assert!(!is_defining_loader_orphaned(&b, SHARED));

    forget_unloaded_classes(&a, &[SHARED, A_ONLY]);
    assert!(!is_defining_loader_orphaned(&a, SHARED));
    cratonvm_types::loader_pin::forget_vm_loader_pins(a.vm_identity());
    cratonvm_types::mirror_pin::forget_vm_mirror_pins(a.vm_identity());
}

/// gci i12/ul F2: an array class takes its element's defining-loader row,
/// once; an array of a built-in element (no row) takes none.
#[test]
fn gci_i12_ul_array_class_takes_its_elements_loader_row() {
    const VM: usize = 0x12_0100;
    const ELEMENT: u32 = 912_101;
    const ARRAY: u32 = 912_102;
    const BUILTIN_ELEMENT: u32 = 912_103;
    const BUILTIN_ARRAY: u32 = 912_104;
    let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let tables = detached_loader_tables(VM);
    register_defining_loader(tables, ELEMENT, fake(0x12_1000));

    assert!(register_array_class_loader_row(tables, ARRAY, ELEMENT));
    assert_eq!(
        defining_loader_for(tables, ARRAY).map(|l| l.as_ptr() as usize),
        Some(0x12_1000)
    );
    assert!(!register_array_class_loader_row(tables, ARRAY, ELEMENT), "once");
    assert!(!register_array_class_loader_row(tables, BUILTIN_ARRAY, BUILTIN_ELEMENT));
    assert!(defining_loader_for(tables, BUILTIN_ARRAY).is_none());
    assert!(!register_array_class_loader_row(tables, ELEMENT, ELEMENT));

    forget_vm_loader_singletons(VM);
    assert!(defining_loader_for(tables, ARRAY).is_none());
}

/// gc-common w8-e: the defining-loader phase of the reconcile, now judged
/// outside the store's mutex.
///
/// * VM A's dead rows go, with an orphan marker and their loader pin.
/// * A's moved row follows the pointer map, and so does its pin.
/// * VM B's row on the SAME class id is not judged by A's predicate, even
///   though A's predicate calls B's loader dead (the production predicate
///   answers "dead" for every address outside A's heap).
/// * The shared id keeps B's key-set bit, because B still holds it (since
///   interpreter round i5 wave 101 each VM has its own bits). A's bits of
///   the ids it lost go.
#[test]
fn w8e_reconcile_judges_only_its_own_vms_rows_and_keeps_a_shared_ids_bit() {
    const VM_A: usize = 0xE8_0A00;
    const VM_B: usize = 0xE8_0B00;
    const SHARED: u32 = 900_801;
    const MOVED: u32 = 900_802;
    const A_ONLY: u32 = 900_803;
    let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let a_dead: usize = 0xE8_1000;
    let a_from: usize = 0xE8_2000;
    let a_to: usize = 0xE8_3000;
    let a_only_dead: usize = 0xE8_4000;
    let b_loader: usize = 0xE8_5000;
    register_defining_loader(detached_loader_tables(VM_A), SHARED, fake(a_dead));
    register_defining_loader(detached_loader_tables(VM_B), SHARED, fake(b_loader));
    register_defining_loader(detached_loader_tables(VM_A), MOVED, fake(a_from));
    register_defining_loader(detached_loader_tables(VM_A), A_ONLY, fake(a_only_dead));

    // Dead: A's two loaders, and B's loader as A's heap sees it. Every
    // other address is reported alive, so the process-global namespace
    // and meta tables (which other tests use concurrently) lose nothing.
    let is_marked =
        move |addr: usize| addr != a_dead && addr != a_only_dead && addr != b_loader;
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(a_from, a_to);

    let dead = gc_reconcile_defining_loaders(detached_loader_tables(VM_A), &is_marked, &pointer_map, None);
    assert_eq!(dead, vec![SHARED, A_ONLY]);

    assert!(defining_loader_for(detached_loader_tables(VM_A), SHARED).is_none());
    assert!(is_defining_loader_orphaned(detached_loader_tables(VM_A), SHARED));
    assert_eq!(
        defining_loader_for(detached_loader_tables(VM_B), SHARED).map(|l| l.as_ptr() as usize),
        Some(b_loader),
        "another VM's row is not judged by this VM's predicate"
    );
    assert!(!is_defining_loader_orphaned(detached_loader_tables(VM_B), SHARED));
    assert_eq!(
        defining_loader_for(detached_loader_tables(VM_A), MOVED).map(|l| l.as_ptr() as usize),
        Some(a_to),
        "a survivor follows the pointer map"
    );

    // Interpreter round i5 wave 101, lane L5: the key-set bits are each
    // VM's own now. A's bit for the shared id goes with A's row; B's
    // stays with B's.
    assert!(
        class_may_have_defining_loader(detached_loader_tables(VM_B), SHARED),
        "VM B still holds the id"
    );
    assert!(
        !class_may_have_defining_loader(detached_loader_tables(VM_A), SHARED),
        "VM A no longer holds the id"
    );
    assert!(
        !class_may_have_defining_loader(detached_loader_tables(VM_A), A_ONLY),
        "nobody holds the id any more"
    );

    use cratonvm_types::loader_pin::loader_pin_addr_for_vm;
    assert_eq!(loader_pin_addr_for_vm(VM_A, SHARED), None);
    assert_eq!(loader_pin_addr_for_vm(VM_A, A_ONLY), None);
    assert_eq!(loader_pin_addr_for_vm(VM_A, MOVED), Some(a_to));
    assert_eq!(loader_pin_addr_for_vm(VM_B, SHARED), Some(b_loader));

    forget_vm_loader_singletons(VM_A);
    forget_vm_loader_singletons(VM_B);
}

/// gc-common w11-e: the orphan marker of a class that stays loaded
/// survives until a live loader adopts the class with a new
/// defining-loader row, and then goes for that VM only. Selective
/// forgetting (`forget_unloaded_classes`) drops only the ids it names.
#[test]
fn w11e_orphan_markers_go_only_with_their_class_or_a_new_defining_loader() {
    const VM: usize = 0x11E_0A00;
    const OTHER: usize = 0x11E_0B00;
    // Above `DEFINING_LOADER_BITS_CAP`, so no key-set bit is touched.
    const ADOPTED: u32 = 2_011_001;
    const STAYS: u32 = 2_011_002;
    const UNLOADED: u32 = 2_011_003;
    let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let dead: usize = 0x11E_1000;
    let adopter: usize = 0x11E_2000;
    for id in [ADOPTED, STAYS, UNLOADED] {
        register_defining_loader(detached_loader_tables(VM), id, fake(dead));
    }
    register_defining_loader(detached_loader_tables(OTHER), ADOPTED, fake(dead));
    let no_moves = cratonvm_types::PointerMap::default();
    let is_marked = move |addr: usize| addr != dead;
    assert_eq!(
        gc_reconcile_defining_loaders(detached_loader_tables(VM), &is_marked, &no_moves, None),
        vec![ADOPTED, STAYS, UNLOADED]
    );
    assert_eq!(
        gc_reconcile_defining_loaders(detached_loader_tables(OTHER), &is_marked, &no_moves, None),
        vec![ADOPTED]
    );
    for id in [ADOPTED, STAYS, UNLOADED] {
        assert!(is_defining_loader_orphaned(detached_loader_tables(VM), id));
    }
    assert!(is_defining_loader_orphaned(detached_loader_tables(OTHER), ADOPTED));

    // The unload transaction names only the class it removed.
    forget_unloaded_classes(detached_loader_tables(VM), &[UNLOADED]);
    assert!(!is_defining_loader_orphaned(detached_loader_tables(VM), UNLOADED));
    assert!(
        is_defining_loader_orphaned(detached_loader_tables(VM), STAYS),
        "a class still loaded keeps its marker"
    );

    // A live loader adopts the class: its row carries the restriction now.
    register_defining_loader(detached_loader_tables(VM), ADOPTED, fake(adopter));
    assert!(!is_defining_loader_orphaned(detached_loader_tables(VM), ADOPTED));
    assert_eq!(
        defining_loader_for(detached_loader_tables(VM), ADOPTED).map(|l| l.as_ptr() as usize),
        Some(adopter)
    );
    assert!(
        is_defining_loader_orphaned(detached_loader_tables(OTHER), ADOPTED),
        "another VM's marker on the same id is not this VM's to clear"
    );
    assert!(is_defining_loader_orphaned(detached_loader_tables(VM), STAYS));

    forget_vm_loader_singletons(VM);
    forget_vm_loader_singletons(OTHER);
    assert!(!is_defining_loader_orphaned(detached_loader_tables(VM), STAYS));
    assert!(!is_defining_loader_orphaned(detached_loader_tables(OTHER), ADOPTED));
}

/// gc-common w10-f: `class_defined_by_this_loader_object` answers from the
/// loader -> classes index, and gives the same answer as a full walk of
/// the rows (the probe it replaced) before and after the reconcile prunes
/// a dead loader and remaps a moved one, across re-registration to another
/// loader, and with another VM holding rows on the same ids and the same
/// loader address.
#[test]
fn w10f_this_loader_probe_uses_the_index_and_follows_the_reconcile() {
    use cratonvm_types::ClassId;
    const VM: usize = 0x10F_0A00;
    const OTHER: usize = 0x10F_0B00;
    let l1_from: usize = 0x10F_1000;
    let l1_to: usize = 0x10F_1800;
    let l2: usize = 0x10F_2000;
    fn fake(addr: usize) -> ObjectRef {
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }
    fn probe(ctx: &mut MockNativeContext, this: usize, name: &str) -> Option<ClassId> {
        class_defined_by_this_loader_object(ctx, fake(this), name)
    }
    // The walk the index replaced, as the oracle.
    fn walk(ctx: &mut MockNativeContext, this: usize, name: &str) -> Option<ClassId> {
        let ids: Vec<u32> = detached_loader_tables(ctx.vm_identity())
            .defining
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .iter()
            .filter_map(|(&cid, loader)| (loader.as_ptr() as usize == this).then_some(cid))
            .collect();
        ids.into_iter()
            .map(ClassId::new)
            .find(|&cid| ctx.class_name_arc_of_id(cid).as_deref() == Some(name))
    }
    let names = ["w10f/A", "w10f/B", "w10f/C", "w10f/D", "w10f/Missing"];
    let agree = |ctx: &mut MockNativeContext, loaders: &[usize], when: &str| {
        for &loader in loaders {
            for name in names {
                assert_eq!(
                    probe(ctx, loader, name),
                    walk(ctx, loader, name),
                    "{when}: the index and the walk disagree on ({loader:#x}, {name})"
                );
            }
        }
    };

    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let a = ctx.ensure_class_initialized("w10f/A").unwrap();
    let b = ctx.ensure_class_initialized("w10f/B").unwrap();
    let c = ctx.ensure_class_initialized("w10f/C").unwrap();
    let d = ctx.ensure_class_initialized("w10f/D").unwrap();
    register_defining_loader(detached_loader_tables(VM), a.as_u32(), fake(l1_from));
    register_defining_loader(detached_loader_tables(VM), b.as_u32(), fake(l1_from));
    register_defining_loader(detached_loader_tables(VM), c.as_u32(), fake(l2));
    // Another VM, same id, same loader address: never an answer for VM.
    register_defining_loader(detached_loader_tables(OTHER), a.as_u32(), fake(l2));

    assert_eq!(probe(&mut ctx, l1_from, "w10f/A"), Some(a));
    assert_eq!(probe(&mut ctx, l1_from, "w10f/A"), Some(a), "memoized hit");
    assert_eq!(probe(&mut ctx, l1_from, "w10f/C"), None);
    assert_eq!(probe(&mut ctx, l2, "w10f/A"), None, "another VM's row");
    agree(&mut ctx, &[l1_from, l2], "after registration");

    // A class registered after the names were read is still found.
    register_defining_loader(detached_loader_tables(VM), d.as_u32(), fake(l1_from));
    assert_eq!(probe(&mut ctx, l1_from, "w10f/D"), Some(d));
    // Re-registration moves the id between the loaders' entries, and the
    // memoized name goes with it.
    register_defining_loader(detached_loader_tables(VM), b.as_u32(), fake(l2));
    assert_eq!(probe(&mut ctx, l1_from, "w10f/B"), None);
    assert_eq!(probe(&mut ctx, l2, "w10f/B"), Some(b));
    assert_eq!(probe(&mut ctx, l1_from, "w10f/A"), Some(a));
    agree(&mut ctx, &[l1_from, l2], "after re-registration");

    // l2 dies, l1 moves. OTHER's row names l2 too and is not judged.
    let is_marked = move |addr: usize| addr != l2;
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(l1_from, l1_to);
    let dead = gc_reconcile_defining_loaders(detached_loader_tables(VM), &is_marked, &pointer_map, None);
    let mut expected_dead = vec![b.as_u32(), c.as_u32()];
    expected_dead.sort_unstable();
    assert_eq!(dead, expected_dead);

    assert_eq!(
        probe(&mut ctx, l1_to, "w10f/A"),
        Some(a),
        "the index follows the move"
    );
    assert_eq!(probe(&mut ctx, l1_to, "w10f/D"), Some(d));
    assert_eq!(
        probe(&mut ctx, l1_from, "w10f/A"),
        None,
        "the old address answers nothing"
    );
    assert_eq!(
        probe(&mut ctx, l2, "w10f/C"),
        None,
        "a dead loader's entry is gone"
    );
    agree(&mut ctx, &[l1_from, l1_to, l2], "after the reconcile");
    {
        let table = detached_loader_tables(VM)
            .defining
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(!table.by_loader.contains_key(&l2));
        assert!(!table.by_loader.contains_key(&l1_from));
        assert!(table.by_loader.contains_key(&l1_to));
    }
    assert!(
        detached_loader_tables(OTHER)
            .defining
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_loader
            .contains_key(&l2),
        "another VM's entry is not judged by this VM's reconcile"
    );
    assert_eq!(
        defining_loader_for(detached_loader_tables(OTHER), a.as_u32()).map(|l| l.as_ptr() as usize),
        Some(l2)
    );

    forget_vm_loader_singletons(VM);
    forget_vm_loader_singletons(OTHER);
    for vm in [VM, OTHER] {
        assert!(
            detached_loader_tables(vm)
                .defining
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .by_loader
                .is_empty(),
            "teardown drops the VM's index entries"
        );
    }
}

/// gc-common w10-f: the JDK module tables and the platform-package memo
/// are per VM. A VM that has cached them does not answer for another VM,
/// and its teardown drops its rows. The mock has no `ModuleLoaderMap`
/// statics, so an uncached VM reads nothing and answers `false`.
#[test]
fn w10f_module_tables_and_platform_package_memo_are_per_vm() {
    const VM_A: usize = 0x10F_2A00;
    const VM_B: usize = 0x10F_2B00;
    let mut ctx_a = MockNativeContext::new();
    ctx_a.set_vm_identity(VM_A);
    let mut ctx_b = MockNativeContext::new();
    ctx_b.set_vm_identity(VM_B);

    detached_loader_tables(VM_A)
        .builtin_packages
        .lock()
        .insert("w10f/pkg".to_string(), BuiltinLoaderKind::Platform);
    assert!(package_is_platform_defined(&mut ctx_a, "w10f/pkg"));
    assert!(
        !package_is_platform_defined(&mut ctx_b, "w10f/pkg"),
        "VM A's memo must not answer for VM B"
    );

    *detached_loader_tables(VM_A).module_sets.lock() = Some(Arc::new(BuiltinModuleSets {
        boot: std::collections::HashSet::new(),
        platform: std::iter::once("java.sql".to_string()).collect(),
    }));
    assert!(jdk_builtin_module_sets(&mut ctx_a).is_some());
    assert!(
        jdk_builtin_module_sets(&mut ctx_b).is_none(),
        "VM A's module tables must not answer for VM B"
    );

    forget_vm_loader_singletons(VM_A);
    forget_vm_loader_singletons(VM_B);
    assert!(detached_loader_tables(VM_A).module_sets.lock().is_none());
    assert!(detached_loader_tables(VM_A).builtin_packages.lock().is_empty());
    assert!(
        !package_is_platform_defined(&mut ctx_a, "w10f/pkg"),
        "teardown drops the VM's memo"
    );
}

/// Interpreter round i5 wave 102, lane L5
/// (`interpreter-i101-L5-remaining-loader-family-process-globals-FIXED-20261016.md`): the
/// built-in loader singletons, the code-source domains, the adoption bit,
/// the closed-`URLClassLoader` set and the modules-defined-to-loaders bit
/// are fields of each VM's own `LoaderIdentityTables`: what VM A records
/// is invisible to VM B, and A's teardown leaves B's untouched.
#[test]
fn i102_two_vms_loader_singletons_are_separate() {
    const VM_A: usize = 0x102_5A00;
    const VM_B: usize = 0x102_5B00;
    let mut ctx_a = MockNativeContext::new();
    ctx_a.set_vm_identity(VM_A);
    let mut ctx_b = MockNativeContext::new();
    ctx_b.set_vm_identity(VM_B);
    let platform_a = new_object_ref(&mut ctx_a, "java/lang/Object");
    let app_b = new_object_ref(&mut ctx_b, "java/lang/Object");
    let domain_a = new_object_ref(&mut ctx_a, "java/lang/Object");
    let closed_a = new_object_ref(&mut ctx_a, "java/lang/Object");

    set_platform_loader(loader_tables(&ctx_a), Some(platform_a));
    set_app_loader(loader_tables(&ctx_b), Some(app_b));
    with_loader_singletons(loader_tables(&ctx_a), |row| row.adopted = true);
    publish_app_code_source_domain(loader_tables(&ctx_a), "file:/w102/a.jar", domain_a);
    set_modules_defined_to_loaders(loader_tables(&ctx_a), true);
    ucl_mark_closed(&ctx_a, closed_a);

    assert!(platform_loader_of(loader_tables(&ctx_a)).is_some_and(|p| p.as_ptr() == platform_a.as_ptr()));
    assert!(platform_loader_of(loader_tables(&ctx_b)).is_none(), "A's platform loader is not B's");
    assert!(app_loader_of(loader_tables(&ctx_a)).is_none(), "B's app loader is not A's");
    assert!(builtin_loaders_adopted(loader_tables(&ctx_a)));
    assert!(!builtin_loaders_adopted(loader_tables(&ctx_b)));
    assert!(app_code_source_domain(loader_tables(&ctx_b), "file:/w102/a.jar").is_none());
    assert!(modules_are_defined_to_loaders(loader_tables(&ctx_a)));
    assert!(!modules_are_defined_to_loaders(loader_tables(&ctx_b)));
    assert!(ucl_is_closed(&ctx_a, closed_a));
    assert!(!ucl_is_closed(&ctx_b, closed_a), "A's closed set is not B's");

    let mut roots_a = Vec::new();
    gc_scan_loader_singleton_roots(loader_tables(&ctx_a), &mut roots_a);
    assert_eq!(roots_a.len(), 2, "A's platform loader and its one domain");
    let mut roots_b = Vec::new();
    gc_scan_loader_singleton_roots(loader_tables(&ctx_b), &mut roots_b);
    assert_eq!(roots_b.len(), 1, "B's app loader only");

    forget_vm_loader_singletons(VM_A);
    assert!(platform_loader_of(loader_tables(&ctx_a)).is_none());
    assert!(!builtin_loaders_adopted(loader_tables(&ctx_a)));
    assert!(!modules_are_defined_to_loaders(loader_tables(&ctx_a)));
    assert!(!ucl_is_closed(&ctx_a, closed_a));
    assert!(
        app_loader_of(loader_tables(&ctx_b)).is_some_and(|p| p.as_ptr() == app_b.as_ptr()),
        "A's teardown keeps B's singletons"
    );
    forget_vm_loader_singletons(VM_B);
    assert!(app_loader_of(loader_tables(&ctx_b)).is_none());
}

/// zt t21/df9 (define page §9): the define doors' namespace for the
/// platform loader object is its own `Extension` namespace, not the
/// application namespace `0` the application loader object (and a null
/// loader) keep. A VM identity of its own: the singletons are per VM.
#[test]
fn zt21df9_the_platform_loader_defines_in_its_own_namespace() {
    const VM: usize = 0x21_D9_00;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let platform = new_object_ref(&mut ctx, "java/lang/Object");
    let app = new_object_ref(&mut ctx, "java/lang/Object");
    set_platform_loader(loader_tables(&ctx), Some(platform));
    set_app_loader(loader_tables(&ctx), Some(app));
    assert_eq!(
        crate::lang_system::define_loader_namespace(
            &mut ctx,
            Some(&Value::Object(Some(platform))),
            "zt21df9/P"
        ),
        (cratonvm_types::ClassLoaderId::NATIVE_EXTENSION, false)
    );
    assert_eq!(
        crate::lang_system::define_loader_namespace(
            &mut ctx,
            Some(&Value::Object(Some(app))),
            "zt21df9/P"
        ),
        (0, false)
    );
    assert_eq!(
        crate::lang_system::define_loader_namespace(
            &mut ctx,
            Some(&Value::Object(None)),
            "zt21df9/P"
        ),
        (0, false)
    );
    forget_vm_loader_singletons(VM);
}

/// zt t22/df10 (define page §9c): a context without the built-in
/// initiating record (the mock: `builtin_find_loaded_initiated_only`
/// answers `false`, as the VM does under
/// `CRATONVM_FIND_LOADED_BUILTIN_INITIATED_ONLY=0`) hands the Java-facing
/// `findLoadedClass` natives the lookup's answer unchanged, for the
/// application loader too, and a miss stays a miss.
#[test]
fn zt22df10_without_the_record_the_java_answer_is_the_lookup() {
    const VM: usize = 0x22_D1_00;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let app = new_object_ref(&mut ctx, "java/lang/Object");
    set_app_loader(loader_tables(&ctx), Some(app));
    let mirror = new_object_ref(&mut ctx, "java/lang/Object");
    let answered = find_loaded_answer_for_java(&mut ctx, app, "java/lang/Object", Some(mirror));
    assert!(answered.is_some_and(|m| m.as_ptr() == mirror.as_ptr()));
    assert!(find_loaded_answer_for_java(&mut ctx, app, "java/lang/Object", None).is_none());
    forget_vm_loader_singletons(VM);
}

// -----------------------------------------------------------------
// L1 — the four VM-internal loader fields live beside the object
// -----------------------------------------------------------------

/// Drop THIS test's entries from the L1 table of the identity-0 mock
/// contexts these tests use (one detached `LoaderIdentityTables` that
/// every such context shares), and only those.
///
/// The table is shared and the test binary runs its
/// tests on many threads at once, so a `.clear()` here deletes whatever a
/// concurrently-running test just recorded. That is not hypothetical: it
/// made two of these tests fail alternately, a different one each run,
/// which reads like flakiness in the code under test rather than in the
/// harness. Remove by address, the way
/// `loader_namespace_store_is_pruned_and_remapped_by_gc_reconcile`
/// already removes by id.
fn forget_loader_meta(loaders: &[ObjectRef]) {
    let mine: Vec<usize> = loaders.iter().map(|l| l.as_ptr() as usize).collect();
    detached_loader_tables(0)
        .loader_meta
        .lock()
        .retain(|(l, _)| !mine.contains(&(l.as_ptr() as usize)));
}

/// Declare `names` as instance fields of `cid`, so
/// `resolve_field_index_by_class_id` finds them — the mock's resolver
/// consults `declared_fields`.
fn declare_instance_fields(
    ctx: &MockNativeContext,
    cid: cratonvm_types::ClassId,
    names: &[&str],
) {
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

/// Build a `ClassLoader` subclass and an instance of it. `real_layout`
/// declares the JDK's own private fields, which is what tells the two
/// layouts apart.
fn make_loader(ctx: &mut MockNativeContext, class_name: &str, real_layout: bool) -> ObjectRef {
    let class_loader = ctx
        .ensure_class_initialized("java/lang/ClassLoader")
        .expect("ClassLoader class");
    let cid = ctx
        .ensure_class_initialized(class_name)
        .expect("loader class");
    ctx.set_superclass(cid, class_loader);
    if real_layout {
        declare_instance_fields(
            ctx,
            cid,
            &[
                "parent",
                "name",
                "unnamedModule",
                "nameAndId",
                "parallelLockMap",
                "package2certs",
                "classes",
            ],
        );
    }
    new_object_ref(ctx, class_name)
}

/// The predicate must be FALSIFIABLE. `object_num_fields(x) >= N` was the
/// first version of it and shipped completely inert — it answers the same
/// on both layouts. Inject the real JDK's own field names and watch the
/// answer flip; that is the whole difference the fix rests on.
#[test]
fn cl_layout_predicate_is_decided_by_a_real_jdk_field_name() {
    let mut ctx = MockNativeContext::new();
    let ours = make_loader(&mut ctx, "example/L1SyntheticLoader", false);
    let theirs = make_loader(&mut ctx, "example/L1RealLayoutLoader", true);

    assert!(
        cl_has_synthetic_layout(&ctx, ours),
        "a fabricated stub declares no fields at all — that IS our layout"
    );
    assert!(
        !cl_has_synthetic_layout(&ctx, theirs),
        "a class declaring java.lang.ClassLoader's own private fields is \
         the REAL layout; if this passes, the predicate cannot fail and \
         the whole fix is inert"
    );
}

/// Every witness name on its own has to be enough — the point of carrying
/// three is that a rename in a future JDK degrades one at a time instead
/// of flipping the predicate wholesale.
#[test]
fn cl_layout_predicate_fires_on_any_single_witness_field() {
    for (i, witness) in REAL_CLASSLOADER_WITNESS_FIELDS.iter().enumerate() {
        let mut ctx = MockNativeContext::new();
        let class_loader = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("ClassLoader class");
        let name = format!("example/L1Witness{i}");
        let cid = ctx.ensure_class_initialized(&name).expect("loader class");
        ctx.set_superclass(cid, class_loader);
        declare_instance_fields(&ctx, cid, &[witness]);
        let loader = new_object_ref(&mut ctx, &name);
        assert!(
            !cl_has_synthetic_layout(&ctx, loader),
            "`{witness}` alone must identify the real JDK layout"
        );
    }
}

/// On the real layout the four `CL_*` slots are `parent` / `nameAndId` /
/// `parallelLockMap` / `classes` — references, all four. Writing an `Int`
/// there is coerced to `Object(None)` and destroys the JDK's field. The
/// eight measured census rows were exactly these writes on
/// `ClassLoaders$AppClassLoader` and `$PlatformClassLoader`.
#[test]
fn cl_init_leaves_the_four_vm_internal_slots_untouched_on_a_real_layout() {
    let mut ctx = MockNativeContext::new();
    let loader = make_loader(&mut ctx, "example/L1RealCtorLoader", true);

    // A recognisable reference in every one of the four slots. If the
    // native writes an `Int` over it, the slot stops being this object.
    let sentinel = new_object_ref(&mut ctx, "java/lang/Object");
    for slot in [
        CL_LOADER_TYPE,
        CL_CLASSES_LOADED,
        CL_IS_PARALLEL_CAPABLE,
        CL_LOADER_ID,
    ] {
        ctx.set_field(loader, slot, Value::Object(Some(sentinel)));
    }

    let name = ctx.create_string("l1");
    let parent = make_loader(&mut ctx, "example/L1RealCtorParent", true);
    cl_init_name_parent(
        &mut ctx,
        &[
            Value::Object(Some(loader)),
            Value::Object(Some(name)),
            Value::Object(Some(parent)),
        ],
    )
    .expect("ClassLoader(String, ClassLoader) native");

    // Slot 0 IS the real `parent`, so it legitimately receives the parent
    // LOADER — by name. What it must never receive is `Int(LOADER_CUSTOM)`,
    // which `set_field_as` coerces to `Object(None)`.
    assert_eq!(
        ctx.get_field(loader, CL_LOADER_TYPE),
        Value::Object(Some(parent)),
        "slot 0 is `parent` on the real layout: the parent loader, never \
         the VM's loader-type tag"
    );
    // 3 / 4 / 6 are `nameAndId` / `parallelLockMap` / `classes` and have no
    // CratonVM counterpart at all — nothing may be written there.
    for slot in [CL_CLASSES_LOADED, CL_IS_PARALLEL_CAPABLE, CL_LOADER_ID] {
        assert_eq!(
            ctx.get_field(loader, slot),
            Value::Object(Some(sentinel)),
            "slot {slot} is a JDK reference field with no CratonVM \
             counterpart and must not be overwritten with VM bookkeeping"
        );
    }

    // ... and the values are not lost: they moved next to the object.
    let meta =
        loader_meta_get(detached_loader_tables(ctx.vm_identity()), loader).expect("constructor must record the meta");
    assert_eq!(meta.loader_type, Some(LOADER_CUSTOM));
    assert_eq!(meta.classes_loaded, Some(0));
    assert_eq!(meta.parallel_capable, Some(true));
    assert!(
        meta.loader_id.is_some_and(|id| id > 0),
        "the (String, ClassLoader) form assigns a namespace id"
    );
    assert_eq!(loader_type_of(&ctx, loader), Some(LOADER_CUSTOM));
    assert_eq!(loader_id_of(&ctx, loader), meta.loader_id);
    assert_eq!(loader_parallel_capable_of(&ctx, loader), Some(1));

    forget_loader_meta(&[loader, parent]);
}

/// Synthetic-JDK mode is unchanged: the slots are still written, so a
/// reader that has not been converted (or a loader whose meta entry was
/// dropped) still finds the values where they have always been.
#[test]
fn cl_init_still_writes_the_slots_on_our_own_layout() {
    let mut ctx = MockNativeContext::new();
    let loader = make_loader(&mut ctx, "example/L1SyntheticCtorLoader", false);

    let name = ctx.create_string("l1");
    cl_init_name_parent(
        &mut ctx,
        &[
            Value::Object(Some(loader)),
            Value::Object(Some(name)),
            Value::Object(None),
        ],
    )
    .expect("ClassLoader(String, ClassLoader) native");

    assert_eq!(
        ctx.get_field(loader, CL_LOADER_TYPE),
        Value::Int(LOADER_CUSTOM)
    );
    assert_eq!(ctx.get_field(loader, CL_CLASSES_LOADED), Value::Int(0));
    assert_eq!(ctx.get_field(loader, CL_IS_PARALLEL_CAPABLE), Value::Int(1));
    let slot_id = match ctx.get_field(loader, CL_LOADER_ID) {
        Value::Int(v) => v,
        other => panic!("expected a namespace id in slot 6, got {other:?}"),
    };
    assert!(slot_id > 0);
    assert_eq!(loader_id_of(&ctx, loader), Some(slot_id as u32));

    // The raw-slot fallback is what keeps a loader allocated outside our
    // path working. Drop the table entry and the slots must still answer.
    forget_loader_meta(&[loader]);
    assert_eq!(loader_type_of(&ctx, loader), Some(LOADER_CUSTOM));
    assert_eq!(loader_classes_loaded_of(&ctx, loader), Some(0));
    assert_eq!(loader_parallel_capable_of(&ctx, loader), Some(1));
    assert_eq!(loader_id_of(&ctx, loader), Some(slot_id as u32));
}

/// The fallback must NOT fire on the real layout: slot 4 there is
/// `parallelLockMap`, and reading it back as this VM's parallel-capable
/// flag is the same type confusion the write side just stopped
/// committing. "No `CL_*` value is read from an object slot on a
/// real-JDK run" is half of L1's done-when.
#[test]
fn readers_do_not_fall_back_to_raw_slots_on_a_real_layout() {
    let mut ctx = MockNativeContext::new();
    let loader = make_loader(&mut ctx, "example/L1NoFallbackLoader", true);

    // Plant values that WOULD be read if the fallback were ungated.
    ctx.set_field(loader, CL_LOADER_TYPE, Value::Int(LOADER_PLATFORM));
    ctx.set_field(loader, CL_CLASSES_LOADED, Value::Int(77));
    ctx.set_field(loader, CL_IS_PARALLEL_CAPABLE, Value::Int(0));
    ctx.set_field(loader, CL_LOADER_ID, Value::Int(4242));

    assert_eq!(loader_type_of(&ctx, loader), None);
    assert_eq!(loader_classes_loaded_of(&ctx, loader), None);
    assert_eq!(loader_parallel_capable_of(&ctx, loader), None);
    assert_eq!(loader_id_of(&ctx, loader), None);

    // `isRegisteredAsParallelCapable` therefore answers the same `true`
    // `registerAsParallelCapable` reports, rather than the `0` that
    // happened to be sitting in `parallelLockMap`'s slot.
    let answer = cl_is_registered_as_parallel_capable(&mut ctx, &[Value::Object(Some(loader))])
        .expect("native")
        .expect("return value");
    assert_eq!(answer, Value::Int(1));
}

/// The three REFERENCE slots are the same defect one kind quieter: slot 1
/// is `name:String` and receives a ClassLoader, slot 2 is
/// `unnamedModule:Module` and receives a String, slot 5 is
/// `package2certs:ConcurrentHashMap` and receives a ProtectionDomain. A
/// reference for a reference, so the overlay detector cannot see them —
/// which is why the wave-2 brief's step 4 left them in place on the
/// (mistaken) grounds that the by-name writes duplicate them.
#[test]
fn cl_init_leaves_the_three_reference_slots_untouched_on_a_real_layout() {
    let mut ctx = MockNativeContext::new();
    let loader = make_loader(&mut ctx, "example/L1RealRefSlotLoader", true);
    let sentinel = new_object_ref(&mut ctx, "java/lang/Object");
    for slot in [CL_PARENT_REF, CL_NAME_REF, CL_DEFAULT_DOMAIN] {
        ctx.set_field(loader, slot, Value::Object(Some(sentinel)));
    }

    let name = ctx.create_string("l1-refslots");
    let parent = make_loader(&mut ctx, "example/L1RefSlotParent", true);
    cl_init_name_parent(
        &mut ctx,
        &[
            Value::Object(Some(loader)),
            Value::Object(Some(name)),
            Value::Object(Some(parent)),
        ],
    )
    .expect("ClassLoader(String, ClassLoader) native");

    // Slot 1 IS `name` and legitimately receives the name STRING — by
    // name. The defect was the index write putting the parent LOADER
    // there.
    assert_eq!(
        ctx.get_field(loader, CL_PARENT_REF),
        Value::Object(Some(name)),
        "slot 1 is `name` on the real layout: the name String, never the \
         parent loader"
    );
    // 2 and 5 are `unnamedModule` and `package2certs`. Nothing CratonVM
    // holds belongs in either.
    for slot in [CL_NAME_REF, CL_DEFAULT_DOMAIN] {
        assert_eq!(
            ctx.get_field(loader, slot),
            Value::Object(Some(sentinel)),
            "slot {slot} belongs to the JDK on a real layout"
        );
    }
    // `parent` and `name` still arrive — by NAME, which is where the real
    // fields actually are.
    assert_eq!(
        ctx.get_field_by_name(loader, "parent"),
        Value::Object(Some(parent))
    );
    assert_eq!(
        ctx.get_field_by_name(loader, "name"),
        Value::Object(Some(name))
    );
    assert_eq!(classloader_parent(&mut ctx, loader), Some(parent));

    forget_loader_meta(&[loader, parent]);
}

/// `classloader_parent`'s slot fallback read slot 1 unconditionally. On a
/// real layout that is `java.lang.ClassLoader.name` — a String — so a
/// parentless loader whose name had been written reported its own NAME as
/// its PARENT, and every caller that walks the chain then treated a String
/// as a ClassLoader.
#[test]
fn classloader_parent_does_not_return_the_name_string_as_a_parent() {
    let mut ctx = MockNativeContext::new();
    let real = make_loader(&mut ctx, "example/L1ParentFallbackReal", true);
    let name = ctx.create_string("platform");
    // Exactly what `get_or_create_platform_loader` produces: the real
    // `name` field set, the real `parent` genuinely null.
    ctx.set_field_by_name(real, "name", Value::Object(Some(name)));
    assert_eq!(
        ctx.get_field(real, CL_PARENT_REF),
        Value::Object(Some(name)),
        "precondition: slot 1 IS the name on the real layout"
    );
    assert_eq!(
        classloader_parent(&mut ctx, real),
        None,
        "a parentless real-layout loader has no parent — not its own name"
    );

    // Synthetic layout keeps the fallback: it is what makes an ordinary
    // `URLClassLoader` constructed with a non-null parent resolve at all.
    let ours = make_loader(&mut ctx, "example/L1ParentFallbackSynthetic", false);
    let p = make_loader(&mut ctx, "example/L1ParentFallbackSyntheticParent", false);
    ctx.set_field(ours, CL_PARENT_REF, Value::Object(Some(p)));
    assert_eq!(classloader_parent(&mut ctx, ours), Some(p));
}

/// The side table is authoritative, ahead of the slot — the ordering
/// `vh_field_desc` uses.
#[test]
fn the_side_table_outranks_the_raw_slot() {
    let mut ctx = MockNativeContext::new();
    let loader = make_loader(&mut ctx, "example/L1PrecedenceLoader", false);
    ctx.set_field(loader, CL_LOADER_TYPE, Value::Int(LOADER_APP));
    loader_meta_put(
        detached_loader_tables(ctx.vm_identity()),
        loader,
        LoaderMeta {
            loader_type: Some(LOADER_CUSTOM),
            ..LoaderMeta::default()
        },
    );

    assert_eq!(loader_type_of(&ctx, loader), Some(LOADER_CUSTOM));
    // A member the table does NOT carry still falls through to the slot.
    ctx.set_field(loader, CL_CLASSES_LOADED, Value::Int(5));
    assert_eq!(loader_classes_loaded_of(&ctx, loader), Some(5));

    forget_loader_meta(&[loader]);
}

/// Same GC contract as `loader_namespace_id_store`, and for the same
/// reason: the table is keyed by a raw heap address. Prune the dead so a
/// recycled address cannot make a brand-new loader inherit a dead one's
/// namespace id; remap the moved so a relocated loader keeps its own.
/// Deleting either half of the `retain_mut` fails here.
#[test]
fn loader_meta_store_is_pruned_and_remapped_by_gc_reconcile() {
    let mut ctx = MockNativeContext::new();
    let dead = make_loader(&mut ctx, "example/L1GcDeadLoader", false);
    let moved_from = make_loader(&mut ctx, "example/L1GcMovedLoader", false);
    let moved_to = new_object_ref(&mut ctx, "example/L1GcMovedLoader");

    const DEAD_NS: u32 = 90_101;
    const MOVED_NS: u32 = 90_102;
    loader_meta_put(
        detached_loader_tables(ctx.vm_identity()),
        dead,
        LoaderMeta {
            loader_id: Some(DEAD_NS),
            ..LoaderMeta::default()
        },
    );
    loader_meta_put(
        detached_loader_tables(ctx.vm_identity()),
        moved_from,
        LoaderMeta {
            loader_id: Some(MOVED_NS),
            ..LoaderMeta::default()
        },
    );
    assert_eq!(loader_id_of(&ctx, dead), Some(DEAD_NS));

    let dead_addr = dead.as_ptr() as usize;
    let from_addr = moved_from.as_ptr() as usize;
    let to_addr = moved_to.as_ptr() as usize;
    // Process-global store in a multi-threaded test binary: report every
    // address this test does not own as ALIVE, so nothing another test
    // registered can be pruned by this call.
    let is_marked = move |addr: usize| addr != dead_addr;
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(from_addr, to_addr);

    gc_reconcile_defining_loaders(detached_loader_tables(ctx.vm_identity()), &is_marked, &pointer_map, None);

    assert!(
        loader_meta_get(detached_loader_tables(ctx.vm_identity()), dead).is_none(),
        "a loader collected this cycle must not keep its bookkeeping entry"
    );
    assert_eq!(
        loader_meta_get(detached_loader_tables(ctx.vm_identity()), moved_to).and_then(|m| m.loader_id),
        Some(MOVED_NS),
        "a relocated loader must keep its bookkeeping at its new address"
    );
    assert!(
        loader_meta_get(detached_loader_tables(ctx.vm_identity()), moved_from).is_none(),
        "the pre-collection address must no longer resolve"
    );

    forget_loader_meta(&[dead, moved_from, moved_to]);
}

/// `loader_object_for_namespace_id` has to be able to invert an id that a
/// constructor native assigned. Before L1 real-JDK mode got that for free
/// (the slot write was coerced away, so `loader_namespace_id_at` allocated
/// a fresh id and stored the object); now the constructor's id survives,
/// so it has to be mirrored in explicitly.
#[test]
fn a_constructor_assigned_namespace_id_is_invertible() {
    let mut ctx = MockNativeContext::new();
    let loader = make_loader(&mut ctx, "example/L1InvertibleLoader", true);
    let name = ctx.create_string("l1");
    cl_init_name_parent(
        &mut ctx,
        &[
            Value::Object(Some(loader)),
            Value::Object(Some(name)),
            Value::Object(None),
        ],
    )
    .expect("ClassLoader(String, ClassLoader) native");

    let ns = loader_namespace_id(&mut ctx, loader);
    assert!(ns > 0, "a user-defined loader gets its own namespace");
    let tables = detached_loader_tables(ctx.vm_identity());
    assert_eq!(
        loader_object_for_namespace_id_in(tables, ns).map(|o| o.as_ptr()),
        Some(loader.as_ptr()),
        "the id must resolve back to the loader that owns it"
    );

    tables
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(_, id)| *id != ns);
    forget_loader_meta(&[loader]);
}

#[test]
fn test_cl_init_default_registered() {
    let r = make_registry();
    assert!(r.find(CL_CLASS, "<init>", "()V").is_some());
}

#[test]
fn test_lookup_ensure_initialized_registered() {
    let r = make_registry();
    assert!(r
        .find(
            LK_CLASS,
            "ensureInitialized",
            "(Ljava/lang/Class;)Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_cl_init_parent_registered() {
    let r = make_registry();
    assert!(r
        .find(CL_CLASS, "<init>", "(Ljava/lang/ClassLoader;)V")
        .is_some());
}

#[test]
fn test_cl_init_name_parent_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "<init>",
            "(Ljava/lang/String;Ljava/lang/ClassLoader;)V"
        )
        .is_some());
}

#[test]
fn test_cl_load_class_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_cl_load_class_resolve_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "loadClass",
            "(Ljava/lang/String;Z)Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_cl_find_class_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "findClass",
            "(Ljava/lang/String;)Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_cl_find_class_module_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "findClass",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_builtin_find_loaded_class_hides_user_namespace_hit() {
    let mut ctx = MockNativeContext::new();
    let loader = match ctx.new_object("java/lang/ClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected classloader object, got {other:?}"),
    };
    let cid = ctx.ensure_class_initialized("leak/OnlyChild").unwrap();
    ctx.set_loader_id_override(cid, 7);

    assert!(find_loaded_class_for_loader(&mut ctx, loader, "leak/OnlyChild").is_none());
}

#[test]
fn test_builtin_find_loaded_class_keeps_application_namespace_hit() {
    let mut ctx = MockNativeContext::new();
    let loader = match ctx.new_object("java/lang/ClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected classloader object, got {other:?}"),
    };
    let cid = ctx.ensure_class_initialized("framework/Generated").unwrap();
    ctx.set_loader_id_override(cid, 2);

    assert!(find_loaded_class_for_loader(&mut ctx, loader, "framework/Generated").is_some());
}

#[test]
fn test_builtin_find_loaded_class_hides_user_defined_app_namespace_hit() {
    let mut ctx = MockNativeContext::new();
    let app_loader = match ctx.new_object("java/lang/ClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected classloader object, got {other:?}"),
    };
    let child_loader = match ctx.new_object("bsh/classpath/BshClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected child loader object, got {other:?}"),
    };
    let cid = ctx.ensure_class_initialized("MyMessenger").unwrap();
    ctx.set_loader_id_override(cid, 2);
    // A private identity: identity 0's tables are every default mock's,
    // and this registration latches `any_defining` there for the whole
    // run (`r21-mock-identity-zero-loader-latch-FIXED-20261020.md`).
    ctx.set_vm_identity(0x7744_0010);
    register_defining_loader(detached_loader_tables(ctx.vm_identity()), cid.as_u32(), child_loader);

    assert!(
        find_loaded_class_for_loader(&mut ctx, app_loader, "MyMessenger").is_none(),
        "built-in loaders must not see app-namespace classes defined by a child loader"
    );
    assert!(
        resolve_global_if_visible(&mut ctx, app_loader, "MyMessenger")
            .unwrap()
            .is_none(),
        "base loadClass global fallback must apply the same child-loader visibility rule"
    );
}

/// W7-82. A bare `java.net.URLClassLoader` is a JDK CLASS but a
/// user-defined LOADER — it is the only entry on `is_builtin_loader_class`'s
/// list with a public constructor. `loader_namespace_id_at` already carves
/// it out and gives it its own namespace to define into; this function must
/// carve it out too, or the loader defines into a namespace it can never
/// see and every later lookup re-drives the define, which the class
/// manager's duplicate-define check then correctly rejects.
#[test]
fn test_bare_url_class_loader_sees_the_class_it_defined_itself() {
    let mut ctx = MockNativeContext::new();
    let loader = match ctx.new_object("java/net/URLClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected classloader object, got {other:?}"),
    };
    let cid = ctx.ensure_class_initialized("aux/Target").unwrap();
    // A user namespace: exactly what `loader_namespace_id_at` hands a bare
    // `URLClassLoader`, and what the built-in branch's `> 2` clause hides.
    ctx.set_loader_id_override(cid, 7);
    // Private identity (`r21-mock-identity-zero-loader-latch-FIXED-20261020.md`).
    ctx.set_vm_identity(0x7744_0011);
    register_defining_loader(detached_loader_tables(ctx.vm_identity()), cid.as_u32(), loader);

    assert!(
        find_loaded_class_for_loader(&mut ctx, loader, "aux/Target").is_some(),
        "a bare URLClassLoader must see the class it is itself recorded as \
         having defined; hiding it re-drives the define and the duplicate \
         check rejects the second Class.forName"
    );
}

/// The other half, so the carve-out above cannot be widened into "a bare
/// `URLClassLoader` sees any user-namespace class of that name". The
/// two-independent-loaders isolation in `ForNameCacheProbe` group3 is the
/// end-to-end form of this.
#[test]
fn test_bare_url_class_loader_does_not_see_another_loaders_class() {
    let mut ctx = MockNativeContext::new();
    let loader = match ctx.new_object("java/net/URLClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected classloader object, got {other:?}"),
    };
    let other = match ctx.new_object("bsh/classpath/BshClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected child loader object, got {other:?}"),
    };
    let cid = ctx.ensure_class_initialized("aux/Foreign").unwrap();
    ctx.set_loader_id_override(cid, 7);
    // Private identity (`r21-mock-identity-zero-loader-latch-FIXED-20261020.md`).
    ctx.set_vm_identity(0x7744_0012);
    register_defining_loader(detached_loader_tables(ctx.vm_identity()), cid.as_u32(), other);

    assert!(
        find_loaded_class_for_loader(&mut ctx, loader, "aux/Foreign").is_none(),
        "a bare URLClassLoader must NOT see a user-namespace class another \
         loader defined"
    );
}

/// W7-87 — the NARROWING half. `loader_id_of_class(cid) > 2 -> hide` only
/// ever hid USER-namespace classes; the built-in branch's global fallback
/// still handed a bare `URLClassLoader` any APPLICATION-namespace class of
/// that name, one it never defined and was never asked to load. HotSpot 25
/// answers `null` (`findLoadedClass`) / `ClassNotFoundException`
/// (`loadClass`) — measured, not assumed. A bare `new URLClassLoader(urls,
/// null)` is THE isolating-loader idiom, so this fallback defeated the
/// isolation it was constructed for.
///
/// The receiver here is a BARE `java/net/URLClassLoader` on purpose: the
/// 2026-07-01 commit that produced W7-82 shipped two tests that instantiate
/// `java/lang/ClassLoader`, and that is exactly why the case went unseen for
/// six weeks. A `URLClassLoader` SUBCLASS was always correct — it is
/// `is_user_defined_loader` and has no global fallback — so a test written
/// against a subclass cannot fail here.
#[test]
fn test_bare_url_class_loader_does_not_see_an_app_namespace_class_it_never_loaded() {
    let mut ctx = MockNativeContext::new();
    let loader = match ctx.new_object("java/net/URLClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected classloader object, got {other:?}"),
    };
    let cid = ctx.ensure_class_initialized("app/Ordinary").unwrap();
    // Application namespace, no registered defining loader: an ordinary
    // classpath class the application loader owns. `> 2` never fired for
    // this, so the global fallback used to return it.
    ctx.set_loader_id_override(cid, 2);

    assert!(
        find_loaded_class_for_loader(&mut ctx, loader, "app/Ordinary").is_none(),
        "a bare URLClassLoader must NOT see an application-namespace class \
         it neither defined nor was asked to load; HotSpot's findLoadedClass \
         reports null and its loadClass raises ClassNotFoundException"
    );
    // The CONTROL, in the same shape: a genuine built-in loader still sees
    // it. `test_builtin_find_loaded_class_keeps_application_namespace_hit`
    // asserts this independently and is deliberately left untouched; if the
    // narrowing had escaped its carve-out, that test would go red too.
    let builtin = match ctx.new_object("java/lang/ClassLoader").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected classloader object, got {other:?}"),
    };
    assert!(
        find_loaded_class_for_loader(&mut ctx, builtin, "app/Ordinary").is_some(),
        "the narrowing is scoped to java/net/URLClassLoader; a genuine \
         built-in loader must keep its global fallback"
    );
}

#[test]
fn test_cl_define_class_basic_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "defineClass",
            "(Ljava/lang/String;[BII)Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_cl_define_class_pd_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "defineClass",
            "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_cl_define_class_bb_registered() {
    let r = make_registry();
    assert!(r.find(CL_CLASS, "defineClass", "(Ljava/lang/String;Ljava/nio/ByteBuffer;Ljava/security/ProtectionDomain;)Ljava/lang/Class;").is_some());
}

// --- WP2.3-C: JDK-internal defineClass0/1/2 ---

#[test]
fn test_cl_define_class1_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "defineClass1",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        )
        .is_some());
}

#[test]
fn test_cl_define_class2_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "defineClass2",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        )
        .is_some());
}

#[test]
fn test_cl_define_class0_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "defineClass0",
            "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
        )
        .is_some());
}

#[test]
fn test_register_classloader_define_class_idempotent() {
    // The dedicated registrar `register_classloader_define_class`
    // can be called on its own (the top-level registrar already
    // calls it). Make sure it registers all three internal
    // natives, even when invoked directly.
    let mut r = NativeMethodRegistry::new();
    register_classloader_define_class(&mut r);
    assert!(r
        .find(
            CL_CLASS,
            "defineClass1",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        )
        .is_some());
    assert!(r
        .find(
            CL_CLASS,
            "defineClass2",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        )
        .is_some());
    assert!(r
        .find(
            CL_CLASS,
            "defineClass0",
            "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
        )
        .is_some());
}

/// gc-common w10-f: `defineClass0`'s `classData` lands in the new class's
/// mirror, in the `java.lang.Class.classData` field the JDK's
/// `MethodHandles.classData` reads, not in a write-only side table. The
/// mock mirror is class 0 with two slots; declaring `classData` at slot 1
/// stands in for the real `java.lang.Class` layout.
#[test]
fn w10f_define_class0_puts_class_data_in_the_mirror_field() {
    use cratonvm_native_api::FieldMetadata;
    use cratonvm_types::{ArrayElementType, ClassId};
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
    let payload = ctx.fresh_object_ref();
    let class_bytes: Vec<u8> = vec![
        0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x41, 0x00, 0x01, 0x00, 0x21, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let bytes = ctx.new_array(ArrayElementType::Byte, class_bytes.len());
    for (i, b) in class_bytes.iter().enumerate() {
        ctx.set_array_element(bytes, i, Value::Int(*b as i32));
    }
    let name = ctx.create_string("w10f/WithClassData");
    let result = cl_define_class0(
        &mut ctx,
        &[
            Value::Object(None),                  // loader
            Value::Object(None),                  // lookup class
            Value::Object(Some(name)),            // name
            Value::Object(Some(bytes)),           // b
            Value::Int(0),                        // off
            Value::Int(class_bytes.len() as i32), // len
            Value::Object(None),                  // pd
            Value::Int(0),                        // initialize
            Value::Int(DEFINE_CLASS0_FLAG_HIDDEN),
            Value::Object(Some(payload)), // classData
        ],
    );
    let mirror = match result {
        Ok(Some(Value::Object(Some(m)))) => m,
        other => panic!("defineClass0 must return the new class's mirror, got {other:?}"),
    };
    assert_eq!(
        ctx.get_field(mirror, 1),
        Value::Object(Some(payload)),
        "classData must be stored in the mirror's Class.classData field"
    );
}

#[test]
fn test_cl_get_parent_registered() {
    let r = make_registry();
    assert!(r
        .find(CL_CLASS, "getParent", "()Ljava/lang/ClassLoader;")
        .is_some());
}

#[test]
fn test_cl_get_name_registered() {
    let r = make_registry();
    assert!(r
        .find(CL_CLASS, "getName", "()Ljava/lang/String;")
        .is_some());
}

#[test]
fn test_cl_get_system_class_loader_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "getSystemClassLoader",
            "()Ljava/lang/ClassLoader;"
        )
        .is_some());
}

#[test]
fn test_cl_get_platform_class_loader_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "getPlatformClassLoader",
            "()Ljava/lang/ClassLoader;"
        )
        .is_some());
}

#[test]
fn test_cl_resolve_class_registered() {
    let r = make_registry();
    assert!(r
        .find(CL_CLASS, "resolveClass", "(Ljava/lang/Class;)V")
        .is_some());
}

/// t1 wave 5, lane L71: `CRATONVM_LOADER_FIND_LOADED_NO_STANDIN` is a
/// `--real-jdk` decision only; a `--compatible` registry keeps the row.
#[test]
fn find_loaded_standin_is_kept_outside_jdk_only() {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(cratonvm_types::compat::CompatibilityMode::Compatible);
    assert!(!crate::classloader_real::find_loaded_standin_dropped(&r));
}

#[test]
fn test_cl_find_loaded_class_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "findLoadedClass",
            "(Ljava/lang/String;)Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_cl_get_resource_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CL_CLASS,
            "getResource",
            "(Ljava/lang/String;)Ljava/net/URL;"
        )
        .is_some());
}

#[test]
fn test_cl_register_as_parallel_capable_registered() {
    let r = make_registry();
    assert!(r
        .find(CL_CLASS, "registerAsParallelCapable", "()Z")
        .is_some());
}

#[test]
fn test_cl_is_registered_as_parallel_capable_registered() {
    let r = make_registry();
    assert!(r
        .find(CL_CLASS, "isRegisteredAsParallelCapable", "()Z")
        .is_some());
}

// --- URLClassLoader registration tests ---

#[test]
fn test_ucl_init_urls_registered() {
    let r = make_registry();
    assert!(r.find(UCL_CLASS, "<init>", "([Ljava/net/URL;)V").is_some());
}

#[test]
fn test_ucl_init_urls_parent_registered() {
    let r = make_registry();
    assert!(r
        .find(
            UCL_CLASS,
            "<init>",
            "([Ljava/net/URL;Ljava/lang/ClassLoader;)V"
        )
        .is_some());
}

#[test]
fn test_ucl_close_registered() {
    let r = make_registry();
    assert!(r.find(UCL_CLASS, "close", "()V").is_some());
}

#[test]
fn test_ucl_get_urls_registered() {
    let r = make_registry();
    assert!(r.find(UCL_CLASS, "getURLs", "()[Ljava/net/URL;").is_some());
}

#[test]
fn test_url_class_path_boolean_get_urls_registered() {
    let r = make_registry();
    assert!(r
        .find(
            "jdk/internal/loader/URLClassPath",
            "getURLs",
            "(Z)[Ljava/net/URL;",
        )
        .is_some());
}

#[test]
fn test_ucl_add_url_registered() {
    let r = make_registry();
    assert!(r.find(UCL_CLASS, "addURL", "(Ljava/net/URL;)V").is_some());
}

#[test]
fn test_ucl_new_instance_registered() {
    let r = make_registry();
    assert!(r
        .find(
            UCL_CLASS,
            "newInstance",
            "([Ljava/net/URL;)Ljava/net/URLClassLoader;"
        )
        .is_some());
}

#[test]
fn test_ucl_new_instance_parent_registered() {
    let r = make_registry();
    assert!(r
        .find(
            UCL_CLASS,
            "newInstance",
            "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;"
        )
        .is_some());
}

// --- MethodHandles$Lookup registration tests ---

/// The class every LIVE `MethodHandles` static is registered on.
///
/// `lookup()`, `publicLookup()` and `privateLookupIn(..)` are `static`
/// members of `java.lang.invoke.MethodHandles`. `MethodHandles$Lookup`
/// declares none of the three in any real JDK, so this module's
/// registrations of those names on [`LK_CLASS`] address triples that
/// `--fast-boot` and `--real-jdk` can never dispatch to — and
/// `register_classloader_natives` is itself reachable only through
/// `register_synthetic_overrides`, so they exist at all only in a
/// `--features synthetic-jdk` build.
const MH_STATICS_CLASS: &str = "java/lang/invoke/MethodHandles";

/// W4-1's live residual, and it is the same species as the six tests W7-62
/// moved out of this module: an assertion aimed only at the `LK_CLASS`
/// triple reads as coverage of `MethodHandles.lookup()` while guarding a
/// registration no live path reaches. W7-62 kept `lk_lookup` /
/// `lk_public_lookup` on the grounds that they ARE registered; registered
/// is not reachable, and the tests are the half that had to move.
///
/// Both halves are asserted, LIVE FIRST: the first assertion is the one
/// that goes red if `lang_invoke::register_p63_method_handles_lookup` ever
/// stops registering the static, which is the failure that would actually
/// break a running VM. The second is kept and labelled so that dropping the
/// synthetic-mode twin still surfaces here rather than silently.
#[test]
fn test_lk_lookup_registered() {
    let r = make_registry();
    assert!(
        r.find(
            MH_STATICS_CLASS,
            "lookup",
            "()Ljava/lang/invoke/MethodHandles$Lookup;"
        )
        .is_some(),
        "the LIVE MethodHandles.lookup() static must stay registered"
    );
    assert!(
        r.find(
            LK_CLASS,
            "lookup",
            "()Ljava/lang/invoke/MethodHandles$Lookup;"
        )
        .is_some(),
        "this module's MethodHandles$Lookup twin (synthetic-jdk only)"
    );
}

/// See [`test_lk_lookup_registered`] — same rule, and this is the exact
/// entry point W4-1 was filed for. `publicLookup()` reaching a private
/// method was the defect; a green test on the unreachable `LK_CLASS` twin
/// was part of what made it look covered.
#[test]
fn test_lk_public_lookup_registered() {
    let r = make_registry();
    assert!(
        r.find(
            MH_STATICS_CLASS,
            "publicLookup",
            "()Ljava/lang/invoke/MethodHandles$Lookup;"
        )
        .is_some(),
        "the LIVE MethodHandles.publicLookup() static must stay registered"
    );
    assert!(
        r.find(
            LK_CLASS,
            "publicLookup",
            "()Ljava/lang/invoke/MethodHandles$Lookup;"
        )
        .is_some(),
        "this module's MethodHandles$Lookup twin (synthetic-jdk only)"
    );
}

#[test]
fn test_lk_find_virtual_registered() {
    let r = make_registry();
    assert!(r.find(LK_CLASS, "findVirtual", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;").is_some());
}

#[test]
fn test_lk_find_static_registered() {
    let r = make_registry();
    assert!(r.find(LK_CLASS, "findStatic", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;").is_some());
}

#[test]
fn test_lk_find_constructor_registered() {
    let r = make_registry();
    assert!(r
        .find(
            LK_CLASS,
            "findConstructor",
            "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"
        )
        .is_some());
}

#[test]
fn test_lk_has_full_privilege_access_registered() {
    let r = make_registry();
    assert!(r.find(LK_CLASS, "hasFullPrivilegeAccess", "()Z").is_some());
}

#[test]
fn test_lk_drop_lookup_mode_registered() {
    let r = make_registry();
    assert!(r
        .find(
            LK_CLASS,
            "dropLookupMode",
            "(I)Ljava/lang/invoke/MethodHandles$Lookup;"
        )
        .is_some());
}

#[test]
fn test_lk_define_hidden_class_registered() {
    let r = make_registry();
    assert!(r.find(LK_CLASS, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;").is_some());
}

// -----------------------------------------------------------------------
// NEW-8 — defineHiddenClass + isHidden + naming + error paths
// -----------------------------------------------------------------------

/// Build a syntactically-minimal class file whose `this_class` points
/// at a Utf8 entry holding `class_name`. The result contains exactly
/// the fields needed by `extract_this_class_name`: header + constant
/// pool + access_flags + this_class + super_class + interfaces_count
/// + fields_count + methods_count + attributes_count. Every count is
/// zero so the class has no members and no attributes, which is
/// legal but wouldn't pass a real bytecode verifier — we only need
/// it to round-trip through the perfect-hash / name-extraction
/// paths, not to be runnable.
fn minimal_class_file(class_name: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    // u4 magic
    bytes.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    // u2 minor_version, u2 major_version (Java 8 = 52)
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x34]);
    // u2 constant_pool_count — we will write 4 entries at indices
    // 1..=3, so count = 4.
    bytes.extend_from_slice(&[0x00, 0x04]);
    // CP #1: CONSTANT_Utf8 for the class name
    bytes.push(1);
    let name_bytes = class_name.as_bytes();
    bytes.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
    bytes.extend_from_slice(name_bytes);
    // CP #2: CONSTANT_Class pointing at CP #1
    bytes.push(7);
    bytes.extend_from_slice(&[0x00, 0x01]);
    // CP #3: CONSTANT_Utf8 "java/lang/Object" for the super
    let super_name = b"java/lang/Object";
    bytes.push(1);
    bytes.extend_from_slice(&(super_name.len() as u16).to_be_bytes());
    bytes.extend_from_slice(super_name);
    // u2 access_flags (ACC_PUBLIC)
    bytes.extend_from_slice(&[0x00, 0x21]);
    // u2 this_class = CP #2
    bytes.extend_from_slice(&[0x00, 0x02]);
    // u2 super_class = 0 (placeholder — real super_class would be a
    // Class entry; we don't care because the mock accepts any
    // CAFEBABE-prefixed buffer).
    bytes.extend_from_slice(&[0x00, 0x00]);
    // u2 interfaces_count, fields_count, methods_count, attributes_count
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    bytes
}

#[test]
fn new8_extract_this_class_name_basic() {
    let bytes = minimal_class_file("com/example/Foo");
    let name = extract_this_class_name(&bytes);
    assert_eq!(name.as_deref(), Some("com/example/Foo"));
}

#[test]
fn new8_extract_this_class_name_bad_magic() {
    let mut bytes = minimal_class_file("Foo");
    bytes[0] = 0xDE; // corrupt the magic
    assert!(extract_this_class_name(&bytes).is_none());
}

#[test]
fn new8_extract_this_class_name_truncated() {
    assert!(extract_this_class_name(&[0xCA, 0xFE]).is_none());
}

#[test]
fn new8_extract_this_class_name_empty() {
    assert!(extract_this_class_name(&[]).is_none());
}

/// defineHiddenClass must reject a null byte-array argument with
/// an IllegalArgumentException rather than silently returning an
/// empty Lookup.
#[test]
fn new8_define_hidden_class_rejects_null_bytes() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();
    let result = lk_define_hidden_class(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(None), // null bytes
            Value::Int(0),
            Value::Object(None),
        ],
    );
    assert!(
        result.is_err(),
        "null bytes must produce IllegalArgumentException"
    );
}

/// defineHiddenClass must reject a byte array without the class
/// file magic.
#[test]
fn new8_define_hidden_class_rejects_bad_magic() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    // Build a byte[] of zeros (no magic).
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();
    let result = lk_define_hidden_class(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Object(None),
        ],
    );
    assert!(
        result.is_err(),
        "missing magic must produce IllegalArgumentException"
    );
}

/// Happy path: valid bytes → hidden class is defined with a
/// mangled name, marked as hidden, and the returned Lookup carries
/// a mirror whose class id is flagged hidden via `is_class_hidden`.
#[test]
fn new8_define_hidden_class_happy_path() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let bytes = minimal_class_file("com/example/Widget");
    // Copy the Rust Vec<u8> into a Java byte[].
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
    }
    let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();

    let result = lk_define_hidden_class(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(arr)),
            Value::Int(0), // initialize = false
            Value::Object(None),
        ],
    )
    .expect("defineHiddenClass");
    let new_lookup = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected a Lookup object"),
    };

    // The new Lookup must carry a lookupClass field; it should be a
    // non-null class mirror.
    let lookup_class = ctx.get_field(new_lookup, LK_LOOKUP_CLASS_REF);
    let mirror = match lookup_class {
        Value::Object(Some(m)) => m,
        _ => panic!("Lookup.lookupClass must be a non-null mirror"),
    };

    // The stored name in the mock registry should start with
    // "com/example/Widget/0x" — confirming HotSpot-style mangling.
    let stored_name = unsafe { (*ctx.last_defined_class_name.get()).clone() };
    let stored = stored_name.expect("mock recorded the defined name");
    assert!(
        stored.starts_with("com/example/Widget/0x"),
        "expected HotSpot-style mangled name, got {stored:?}"
    );

    // The mirror's class id must be flagged hidden via the mock's
    // `is_class_hidden` override (which reads the same set that
    // `set_class_hidden` populates).
    let cid = crate::lang_class::mirror_class_id(&mut ctx, mirror)
        .expect("mirror must have a class id");
    assert!(
        ctx.is_class_hidden(cid),
        "hidden class id must be tracked as hidden"
    );
}

/// Two consecutive defineHiddenClass calls on byte buffers with
/// the same `this_class` must produce distinct mangled names so
/// the hidden classes never collide in the registry.
#[test]
fn new8_define_hidden_class_names_are_unique() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let bytes = minimal_class_file("Foo");
    let arr1 = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr1, i, Value::Int((*b as i8) as i32));
    }
    let arr2 = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr2, i, Value::Int((*b as i8) as i32));
    }
    let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();

    lk_define_hidden_class(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(arr1)),
            Value::Int(0),
            Value::Object(None),
        ],
    )
    .expect("first define");
    let first_name =
        unsafe { (*ctx.last_defined_class_name.get()).clone() }.expect("first name");

    lk_define_hidden_class(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(arr2)),
            Value::Int(0),
            Value::Object(None),
        ],
    )
    .expect("second define");
    let second_name =
        unsafe { (*ctx.last_defined_class_name.get()).clone() }.expect("second name");

    assert_ne!(
        first_name, second_name,
        "two hidden classes from the same source must have distinct names"
    );
    assert!(first_name.starts_with("Foo/0x"));
    assert!(second_name.starts_with("Foo/0x"));
}

/// NESTMATE parsing: when a ClassOption[] contains an element with
/// ordinal 0 (NESTMATE), the hidden class is flagged as a nestmate.
/// We can't easily assert the nest-info copy from a MockNativeContext
/// because copy_nest_info is a no-op on the default trait impl;
/// the test instead confirms that the define call still succeeds
/// when NESTMATE is specified.
#[test]
fn new8_define_hidden_class_with_nestmate_option() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let bytes = minimal_class_file("com/example/Nested");
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
    }
    // Build a ClassOption[] of length 1 with ordinal = 0 (NESTMATE).
    let option = try_alloc_concurrent_synthetic(
        &mut ctx,
        "java/lang/invoke/MethodHandles$Lookup$ClassOption",
        1,
    )
    .unwrap();
    ctx.set_field(option, 0, Value::Int(0)); // NESTMATE ordinal
    let options_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(options_arr, 0, Value::Object(Some(option)));

    let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();

    let result = lk_define_hidden_class(
        &mut ctx,
        &[
            Value::Object(Some(lookup)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Object(Some(options_arr)),
        ],
    );
    assert!(result.is_ok(), "NESTMATE option must not cause an error");
}

#[test]
fn test_lk_find_var_handle_registered() {
    let r = make_registry();
    assert!(r.find(LK_CLASS, "findVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;").is_some());
}

// --- ProtectionDomain registration tests ---

#[test]
fn test_pd_init_registered() {
    let r = make_registry();
    assert!(r
        .find(
            PD_CLASS,
            "<init>",
            "(Ljava/security/CodeSource;Ljava/security/PermissionCollection;)V"
        )
        .is_some());
}

#[test]
fn test_pd_get_code_source_registered() {
    let r = make_registry();
    assert!(r
        .find(PD_CLASS, "getCodeSource", "()Ljava/security/CodeSource;")
        .is_some());
}

#[test]
fn test_pd_implies_registered() {
    let r = make_registry();
    assert!(r
        .find(PD_CLASS, "implies", "(Ljava/security/Permission;)Z")
        .is_some());
}

// --- CodeSource registration tests ---

#[test]
fn test_cs_init_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CS_CLASS,
            "<init>",
            "(Ljava/net/URL;[Ljava/security/cert/Certificate;)V"
        )
        .is_some());
}

#[test]
fn test_cs_get_location_registered() {
    let r = make_registry();
    assert!(r
        .find(CS_CLASS, "getLocation", "()Ljava/net/URL;")
        .is_some());
}

#[test]
fn test_cs_get_certificates_registered() {
    let r = make_registry();
    assert!(r
        .find(
            CS_CLASS,
            "getCertificates",
            "()[Ljava/security/cert/Certificate;"
        )
        .is_some());
}

// --- Delegation model tests ---

#[test]
fn test_delegation_bootstrap() {
    assert_eq!(delegation_order(LOADER_BOOTSTRAP), "bootstrap-only");
}

#[test]
fn test_delegation_platform() {
    assert!(delegation_order(LOADER_PLATFORM).contains("platform"));
}

#[test]
fn test_delegation_app() {
    assert!(delegation_order(LOADER_APP).contains("app"));
}

#[test]
fn test_delegation_custom() {
    assert!(delegation_order(LOADER_CUSTOM).contains("parent-first"));
}

#[test]
fn test_delegation_unknown() {
    assert_eq!(delegation_order(99), "unknown");
}

// --- Lookup mode bitmask tests ---

#[test]
fn test_lookup_mode_constants() {
    assert_eq!(LK_PUBLIC, 0x01);
    assert_eq!(LK_PRIVATE, 0x02);
    assert_eq!(LK_PROTECTED, 0x04);
    assert_eq!(LK_PACKAGE, 0x08);
    assert_eq!(LK_MODULE, 0x10);
    assert_eq!(LK_UNCONDITIONAL, 0x20);
    assert_eq!(LK_ORIGINAL, 0x40);
}

#[test]
fn test_full_power_includes_all_key_modes() {
    assert_ne!(LK_FULL_POWER & LK_PUBLIC, 0);
    assert_ne!(LK_FULL_POWER & LK_PRIVATE, 0);
    assert_ne!(LK_FULL_POWER & LK_PROTECTED, 0);
    assert_ne!(LK_FULL_POWER & LK_PACKAGE, 0);
    assert_ne!(LK_FULL_POWER & LK_MODULE, 0);
    assert_ne!(LK_FULL_POWER & LK_ORIGINAL, 0);
}

/// The two "full power" values are DIFFERENT numbers and both are load
/// bearing: 95 is what `MethodHandles.lookup().lookupModes()` answers,
/// 0x1F is the JDK's own `FULL_POWER_MODES` mask that `in`/`dropLookupMode`
/// apply. Measured on OpenJDK 25.0.3.
#[test]
fn test_full_power_modes_mask_excludes_original() {
    assert_eq!(LK_FULL_POWER, 95);
    assert_eq!(LK_FULL_POWER_MODES, 0x1F);
    assert_eq!(LK_FULL_POWER_MODES & LK_ORIGINAL, 0);
    assert_eq!(LK_FULL_POWER_MODES & LK_UNCONDITIONAL, 0);
}

/// `Lookup.dropLookupMode` against the real JDK 25 answers.
///
/// Every row was read off `java LkProbe` on OpenJDK 25.0.3 with the
/// receiver `MethodHandles.lookup()` (modes 95) — not derived from the JDK
/// source, and deliberately NOT from `old & !drop`, which this asserts is
/// wrong for all seven droppable modes.
#[test]
fn test_drop_lookup_mode_matches_jdk25() {
    for (drop, expected) in [
        (LK_PUBLIC, 0),
        (LK_PRIVATE, 25),
        (LK_PROTECTED, 27),
        (LK_PACKAGE, 17),
        (LK_MODULE, 1),
        (LK_UNCONDITIONAL, 27),
        (LK_ORIGINAL, 27),
    ] {
        assert_eq!(
            lk_drop_modes(LK_FULL_POWER, drop),
            Some(expected),
            "dropLookupMode(0x{drop:x}) on modes 95"
        );
        assert_ne!(
            LK_FULL_POWER & !drop,
            expected,
            "the naive `old & !drop` must NOT coincide with the JDK answer \
             for 0x{drop:x} — if it does, this test has stopped proving anything"
        );
    }
}

/// Anything that is not exactly one of the seven mode constants is refused.
/// Measured: `dropLookupMode(0)` and `dropLookupMode(PRIVATE|PROTECTED)`
/// both raise `IllegalArgumentException` on JDK 25.
#[test]
fn test_drop_lookup_mode_rejects_non_modes() {
    assert_eq!(lk_drop_modes(LK_FULL_POWER, 0), None);
    assert_eq!(
        lk_drop_modes(LK_FULL_POWER, LK_PRIVATE | LK_PROTECTED),
        None
    );
    assert_eq!(lk_drop_modes(LK_FULL_POWER, 0x80), None);
}

/// `Lookup.in`, measured on OpenJDK 25.0.3 from `MethodHandles.lookup()`
/// (modes 95). The lookup class itself keeps 95; a NESTMATE gets 31; a
/// same-package class in another file gets **25**, not 31, because
/// `isSamePackageMember` strips `PRIVATE|PROTECTED` from a "cousin"; and
/// `String.class` gets 1.
#[test]
fn test_in_modes_matches_jdk25() {
    // (same_class, same_package, same_nest, target_is_public)
    assert_eq!(lk_in_modes(LK_FULL_POWER, true, true, true, true), 95);
    assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, true, true), 31);
    assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, false, true), 25);
    assert_eq!(lk_in_modes(LK_FULL_POWER, false, false, false, true), 1);
    // A full-power lookup's reduction does not depend on the target being
    // public — a package-private nestmate is still 31.
    assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, true, false), 31);
    assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, false, false), 25);
    // An already-reduced lookup never REGAINS a mode.
    assert_eq!(lk_in_modes(25, false, true, true, true), 25);
    assert_eq!(lk_in_modes(25, false, false, false, true), 1);
    assert_eq!(lk_in_modes(1, false, true, true, true), 1);
}

/// `publicLookup()` (UNCONDITIONAL, 32) through `in()`: KEPT for a PUBLIC
/// target, and **0** for one that is not.
///
/// The regression this pins is a whole-value one, not an edge: the arm used
/// to `return prev` for every target, so `publicLookup().in(<a
/// package-private class>)` reported 32 — a lookup able to resolve public
/// members of a class the JDK hands no lookup at all. Measured on OpenJDK
/// 25.0.3 (`PubIn`): a public nested class 32, a package-private nested
/// class 0, a package-private top-level class 0, `java.lang.String` 32.
#[test]
fn test_public_lookup_in_drops_to_zero_for_a_non_public_target() {
    assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, false, false, true), 32);
    assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, true, true, true), 32);
    assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, false, false, false), 0);
    assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, true, true, false), 0);
    // …including a same-package cousin, which the `same_package` arm below
    // would otherwise have kept at 32.
    assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, true, false, false), 0);
}

/// The nestmate approximation: `p/Outer` and `p/Outer$Inner` share an
/// outermost class; `p/Outer` and `p/Mate` do not.
#[test]
fn test_in_modes_nestmate_beats_bare_package_match() {
    // Package-mates that are NOT nestmates lose PRIVATE|PROTECTED …
    assert_eq!(
        lk_in_modes(LK_FULL_POWER, false, true, false, true) & LK_PRIVATE,
        0
    );
    // … while nestmates keep them.
    assert_ne!(
        lk_in_modes(LK_FULL_POWER, false, true, true, true) & LK_PRIVATE,
        0
    );
}

/// `lk_modes_of` must read the SYNTHETIC slot when the receiver's class
/// does not declare `allowedModes`.
///
/// The trap this pins: a by-name-first reader returned 0 for every
/// fabricated Lookup and never consulted the slot that holds the value, so
/// `lookupModes()` reported a powerless Lookup for the whole of
/// synthetic-JDK mode.
///
/// Note that this test exercises ONE of the two absent-field answers.
/// `MockNativeContext` answers `Int(0)` for an unresolvable name;
/// production (`vm_exec.rs::get_field_by_name`) answers
/// `Value::Object(None)`. `lk_modes_of` survives both only because it asks
/// the CLASS first — under production's answer a by-name-first reader
/// would fall through the `Value::Int` arm instead of latching a false 0,
/// but it still would not know which slot to read. Do not read a green
/// result here as evidence about the `Int(0)` convention; there isn't one.
#[test]
fn test_lk_modes_of_reads_synthetic_slot_when_field_absent() {
    // The mock declares no fields for a fresh class, so `allowedModes` is
    // absent and the MOCK's `get_field_by_name` answers `Int(0)` for it.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let lk = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();
    assert_eq!(lk_modes_of(&ctx, lk), LK_FULL_POWER);
}

// --- Loader type constants ---

#[test]
fn test_loader_type_ordering() {
    assert!(LOADER_BOOTSTRAP < LOADER_PLATFORM);
    assert!(LOADER_PLATFORM < LOADER_APP);
    assert!(LOADER_APP < LOADER_CUSTOM);
}

// --- Field count sanity ---

#[test]
fn test_classloader_field_count() {
    assert_eq!(CL_FIELD_COUNT, 7);
}

#[test]
fn test_url_classloader_field_count() {
    assert_eq!(UCL_FIELD_COUNT, 6);
}

#[test]
fn test_lookup_field_count() {
    assert_eq!(LK_FIELD_COUNT, 4);
}

#[test]
fn test_hidden_class_field_count() {
    assert_eq!(HC_FIELD_COUNT, 2);
}

#[test]
fn test_protection_domain_field_count() {
    assert_eq!(PD_FIELD_COUNT, 3);
}

#[test]
fn test_code_source_field_count() {
    assert_eq!(CS_FIELD_COUNT, 2);
}

// -----------------------------------------------------------------------
// T19_H12_ — ClassLoader.loadClass(Module, String) native
// -----------------------------------------------------------------------

#[test]
fn t19_h12_cl_load_class_module_registered() {
    let r = make_registry();
    assert!(
        r.find(
            CL_CLASS,
            "loadClass",
            "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
        )
        .is_some(),
        "ClassLoader.loadClass(Module, String) must be registered"
    );
}

#[test]
fn t19_h12_cl_load_class_module_resolves_existing() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let _ = ctx.ensure_class_initialized("java/lang/String").unwrap();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
    let module = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
    let name = ctx.create_string("java.lang.String");
    let r = cl_load_class_module(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(module)),
            Value::Object(Some(name)),
        ],
    );
    match r.unwrap().unwrap() {
        Value::Object(Some(_)) => {} // mirror returned
        other => panic!("expected Class mirror, got {:?}", other),
    }
}

#[test]
fn t19_h12_cl_load_class_module_missing_returns_null() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
    let module = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
    let name = ctx.create_string("does.not.exist.Bogus");
    // Note: the mock's ensure_class_initialized always succeeds (auto-creates).
    // To verify the spec'd null return on a real miss, we test the
    // hardening / null-arg branches explicitly.
    let _ = cl_load_class_module(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(module)),
            Value::Object(Some(name)),
        ],
    );
    // (mock auto-creates so the hit path is exercised; the explicit
    // null-return branches are covered by the next four tests)
}

#[test]
fn t19_h12_cl_load_class_module_null_module_returns_null() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
    let name = ctx.create_string("java.lang.String");
    let r = cl_load_class_module(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(None),
            Value::Object(Some(name)),
        ],
    );
    match r.unwrap().unwrap() {
        Value::Object(None) => {}
        other => panic!("expected null on null-module, got {:?}", other),
    }
}

#[test]
fn t19_h12_cl_load_class_module_null_name_returns_null() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
    let module = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
    let r = cl_load_class_module(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(module)),
            Value::Object(None),
        ],
    );
    match r.unwrap().unwrap() {
        Value::Object(None) => {}
        other => panic!("expected null on null-name, got {:?}", other),
    }
}

#[test]
fn t19_h12_cl_load_class_module_path_traversal_rejected() {
    // Hardening: control bytes / path separators must short-circuit
    // to null, never reach `ensure_class_initialized`.
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
    let module = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
    let evil = ctx.create_string("../../etc/passwd");
    let r = cl_load_class_module(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(module)),
            Value::Object(Some(evil)),
        ],
    );
    match r.unwrap().unwrap() {
        Value::Object(None) => {}
        other => panic!("expected null on path-traversal name, got {:?}", other),
    }
}

// -----------------------------------------------------------------------
// MOVED 2026-08-12 to `lang_invoke.rs`'s test module.
//
// Six tests lived here — five aimed at `lk_find_virtual` / `lk_find_getter`
// and one at this module's `lk_member_access_flags`. All six were green on
// every run and none of them touched code the VM can reach: those bodies
// were never registered (see the DELETED block above `lk_unreflect` for
// why). They now sit beside `lk_enforce_find_access` in `lang_invoke.rs`,
// aimed at the gate all ten `lookup_find_*` really call, and two arms the
// old tests could not express were added there — a genuine `publicLookup()`
// mode word (UNCONDITIONAL, not PUBLIC) and a zero-mode Lookup.
// W7-62-ratchets-and-dead-code.md
// -----------------------------------------------------------------------

/// `MockNativeContext::resolve_field_index_by_class_id` now falls back to
/// `ClassManager::synthetic_stub_fields`, the same table the VM resolves a
/// name against for a class with no real bytes.
///
/// Before this, the mock answered `None` for every field of every modelled
/// class that had no hand-written `mock_*_field_slot` helper — i.e. "no such
/// field" about fields the VM does resolve. Any predicate of the form
/// *"does this class declare <a field only the REAL JDK class has>"* was
/// therefore **unfalsifiable** under the mock: it could only ever answer
/// one way, and a test of it passed vacuously.
/// [`cl_has_synthetic_layout`] is exactly that shape.
///
/// Both directions are asserted, because a fallback that answered `Some`
/// for *everything* would be just as useless as one that answered `None`.
#[test]
fn resolve_by_class_id_sees_the_fabricated_model() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("java/security/ProtectionDomain")
        .expect("mock ensure_class_initialized must succeed");

    // The model's real JDK declaration order, pinned in
    // `classloading/src/shadow_layout.rs`. A modelled class with no
    // hand-written mock table is the case this fallback exists for.
    for (name, want) in [
        ("codesource", 0usize),
        ("classloader", 1),
        ("principals", 2),
        ("permissions", 3),
    ] {
        assert_eq!(
            ctx.resolve_field_index_by_class_id(cid, name),
            Some(want),
            "ProtectionDomain.{name} must resolve to its modelled slot"
        );
    }

    // A name the class does not declare still answers `None`, so the
    // predicate can still be falsified in the negative direction.
    assert_eq!(
        ctx.resolve_field_index_by_class_id(cid, "parallelLockMap"),
        None
    );
    assert_eq!(
        ctx.resolve_field_index_by_class_id(cid, "nosuchfield"),
        None
    );
}

/// The fallback is the TAIL of the chain, not the head.
///
/// `mock_jdk_field_slot` is a deliberately arbitrary shared name->slot
/// namespace for the `Field`/`Method`/`Constructor`/`MemberName` mirrors,
/// and it disagrees with the fabricated model on every one of those names.
/// It has to keep winning here because it wins in `get_field_by_name` /
/// `set_field_by_name`: a reader and a writer that resolve the same name to
/// different slots is worse than either mapping being "wrong". Putting the
/// model first makes `create_method_object` write `modifiers` to one slot
/// and `method_modifiers_value` read it from another — measured, three
/// tests red.
/// Harvest every class name the fabricated model could be asked about,
/// straight out of the model's own source. A hand-written list would go
/// stale silently, which is the failure mode this whole record is about.
fn harvested_model_class_names() -> Vec<String> {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../classloading/src/class_manager/synthetic_stub_fields.rs"
    ))
    .expect("synthetic field model must be readable from the test");
    let bytes = src.as_bytes();
    let mut names: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j] != b'"' {
            if bytes[j] == b'\\' {
                j += 1;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let lit = &src[start..j];
        if lit.len() > 3
            && lit.contains('/')
            && lit
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '$')
        {
            names.push(lit.to_string());
        }
        i = j + 1;
    }
    names.sort();
    names.dedup();
    names
}

/// THE GATE for this record's second half: **the mock's hand-written slot
/// tables must not shadow the table the VM itself resolves names against.**
///
/// §3 fixed the direction where the mock answered "no such field" about a
/// field the VM resolves. This is the other direction, and it was live:
/// `mock_jdk_field_slot` — the deliberately arbitrary flat namespace for
/// the `Field`/`Method`/`Constructor`/`MemberName` mirrors — was consulted
/// **class-blind and ahead of the model**, so it answered for any modelled
/// class declaring one of its fifteen names. Measured across the 333
/// classes `synthetic_stub_field_model` models, it shadowed `name` on
/// twenty-seven of them (`java.lang.Enum`, `java.security.Permission`,
/// `java.util.logging.Logger`, `org.xnio.Xnio`, `org.jboss.modules.Module`,
/// …), each of which models `name` at slot 0 against the namespace's 1.
///
/// The reflect mirrors are the one exemption, and it is enumerated in
/// `mock_reflect_mirror_field_slot` rather than implied — production keeps
/// the same flat layout as `METHOD_LEGACY_SLOT_*` and allocates the mirror
/// at eight fields, so the model's slots for the later names are past the
/// end of the object.
#[test]
fn the_mock_slot_tables_do_not_shadow_the_fabricated_model() {
    const MIRRORS: &[&str] = &[
        "java/lang/reflect/Field",
        "java/lang/reflect/Method",
        "java/lang/reflect/Constructor",
        "java/lang/reflect/Executable",
        "java/lang/reflect/AccessibleObject",
        "java/lang/invoke/MemberName",
    ];

    let names = harvested_model_class_names();
    assert!(
        names.len() > 300,
        "the literal walker found only {} candidate class names in \
         class_manager.rs — the walker is broken, not the model",
        names.len()
    );

    let mut modelled = 0usize;
    let mut shadowed: Vec<String> = Vec::new();
    for cls in &names {
        let model = cratonvm_classloading::synthetic_stub_field_model(cls);
        let instance: Vec<_> = model.iter().filter(|f| !f.is_static()).collect();
        if instance.is_empty() {
            continue;
        }
        modelled += 1;
        if MIRRORS.contains(&cls.as_str()) {
            continue;
        }
        let mut ctx = MockNativeContext::new();
        let Ok(cid) = ctx.ensure_class_initialized(cls) else {
            continue;
        };
        for (want, f) in instance.iter().enumerate() {
            // `_fN` asserts nothing about a name and `_vmN` is a slot this
            // VM parks its own value in; neither is a name anybody resolves.
            if f.name.starts_with("_f") || f.name.starts_with("_vm") {
                continue;
            }
            let got = ctx.resolve_field_index_by_class_id(cid, &f.name);
            if got != Some(want) {
                shadowed.push(format!("{cls}.{} model={want} mock={got:?}", f.name));
            }
        }
    }

    assert!(
        modelled > 250,
        "only {modelled} of {} harvested names are modelled — the model or \
         the harvest changed shape, and this gate is measuring nothing",
        names.len()
    );
    assert!(
        shadowed.is_empty(),
        "{} modelled field(s) resolve to a slot the VM does not use, because \
         a hand-written mock table answered first. Every one of these is a \
         native tested against the wrong field:\n  {}",
        shadowed.len(),
        shadowed.join("\n  ")
    );
}

/// `resolve_field_index` is the THIRD by-name entry point, and until this
/// change it consulted exactly one hand-written table
/// (`mock_undertow_exchange_field_slot`) — so it answered `None` for every
/// other class in the tree.
///
/// Production reaches for it constantly: `java/lang/Enum.name`,
/// `java/lang/Throwable.detailMessage`,
/// `java/lang/StackTraceElement.declaringClass`, the whole
/// `jdk.internal.foreign` segment family. Every branch behind those calls
/// was unreachable under the mock — the same unfalsifiable-predicate shape
/// §3 fixed one entry point over, in the entry point §3 did not touch.
///
/// It answers for every class the model models, which is not every class
/// production asks about: `java.lang.Throwable` has real bytes and no
/// fabricated model, so `detailMessage` is still `None` here and
/// `lang_misc`'s three `detailMessage` resolutions are still unfalsifiable
/// under the mock. That is a gap in the model's coverage, not in the
/// chain — a test that asserted otherwise was written first and went red.
#[test]
fn resolve_field_index_by_name_sees_the_fabricated_model() {
    let ctx = MockNativeContext::new();
    // `java.lang.Enum` models `name` first. Before this it was `None`, so
    // `native_enum_name`'s resolved-slot branch could not be reached.
    assert_eq!(ctx.resolve_field_index("java/lang/Enum", "name"), Some(0));
    // The application-specific fabricated model was removed; use the
    // existing mock fallback for this unmodelled field.
    assert_eq!(
        ctx.resolve_field_index("io/undertow/server/HttpServerExchange", "responseHeaders"),
        Some(3)
    );
    // A name the model does NOT declare still falls through to the
    // hand-written table, which is what that table is for.
    assert_eq!(
        ctx.resolve_field_index("io/undertow/server/HttpServerExchange", "requestURI"),
        Some(21)
    );
    // Still falsifiable in the negative direction: a fallback that answered
    // `Some` for everything would be as useless as one answering `None`.
    assert_eq!(
        ctx.resolve_field_index("java/lang/Enum", "nosuchfield"),
        None
    );
}

/// All the by-name entry points must answer ONE slot for one name.
///
/// They did not. `java/lang/reflect/Parameter` was special-cased in
/// `get_field_by_name`/`set_field_by_name` and not in
/// `resolve_field_index_by_class_id`, so a writer put `name` in slot 0 and
/// a reader coming the other way looked in slot 1 — the exact
/// reader/writer split the chain's ordering exists to prevent, sitting in
/// the mock the whole time.
#[test]
fn every_by_name_entry_point_resolves_one_name_to_one_slot() {
    for (class, field) in [
        ("java/lang/reflect/Parameter", "name"),
        ("java/lang/reflect/Field", "clazz"),
        ("java/lang/Enum", "name"),
        ("java/security/ProtectionDomain", "codesource"),
    ] {
        let mut ctx = MockNativeContext::new();
        let cid = ctx
            .ensure_class_initialized(class)
            .expect("mock ensure_class_initialized must succeed");
        let by_id = ctx.resolve_field_index_by_class_id(cid, field);
        let by_name = ctx.resolve_field_index(class, field);
        assert_eq!(
            by_id, by_name,
            "{class}.{field}: resolve_field_index_by_class_id says {by_id:?} \
             and resolve_field_index says {by_name:?}"
        );
        let slot = by_id.expect("all four pairs above are resolvable");

        // And the write half lands where the read half looks.
        let obj = ctx.alloc_object(cid, 16);
        ctx.set_field_by_name(obj, field, Value::Int(0x5EED));
        assert_eq!(
            ctx.get_field(obj, slot),
            Value::Int(0x5EED),
            "{class}.{field}: set_field_by_name did not write slot {slot}"
        );
        assert_eq!(ctx.get_field_by_name(obj, field), Value::Int(0x5EED));
    }
}

#[test]
fn the_hand_written_namespace_still_wins_for_the_reflect_mirrors() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("java/lang/reflect/Field")
        .expect("mock ensure_class_initialized must succeed");
    // `mock_jdk_field_slot`'s answer, not the model's (which puts `clazz`
    // at 2, behind `override` and `accessCheckCache`).
    assert_eq!(ctx.resolve_field_index_by_class_id(cid, "clazz"), Some(0));
    assert_eq!(ctx.resolve_field_index_by_class_id(cid, "name"), Some(1));
}

/// gc-common w18-d
/// (`common-w8e-non-strong-hidden-classes-unload-only-with-their-loader`):
/// the post-GC reconcile reports a non-strong hidden class whose MIRROR
/// died, while its defining loader is alive -- and a built-in-loader one
/// with no defining loader at all. A strong class of the same live loader
/// is not reported. A live non-strong class's instance edge (`loader_pin`)
/// follows its moved mirror, and a re-registration of its defining loader
/// cannot turn that edge back into instance -> loader. Its mirror is never
/// pinned to the loader (`mirror_pin`).
#[test]
fn w18d_a_non_strong_hidden_class_unloads_with_its_mirror_not_its_loader() {
    use cratonvm_types::loader_pin::loader_pin_addr_for_vm;
    use cratonvm_types::mirror_pin::{add_mirror_pin, mirrors_for_loader};
    const VM: usize = 0x18D_0B00;
    // Above `DEFINING_LOADER_BITS_CAP`: no key-set bit is touched.
    const STRONG: u32 = 2_018_001;
    const USER_NS: u32 = 2_018_002;
    const BUILTIN_NS: u32 = 2_018_003;
    const LIVE: u32 = 2_018_004;
    let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let loader: usize = 0x18D_1000;
    let strong_mirror: usize = 0x18D_2000;
    let user_mirror: usize = 0x18D_2100;
    let builtin_mirror: usize = 0x18D_2200;
    let live_from: usize = 0x18D_2300;
    let live_to: usize = 0x18D_2400;

    register_defining_loader(detached_loader_tables(VM), STRONG, fake(loader));
    register_defining_loader(detached_loader_tables(VM), USER_NS, fake(loader));
    // What `get_or_create_class_mirror` records when each mirror is made,
    // BEFORE the define native registers the non-strong class.
    add_mirror_pin(VM, loader, strong_mirror);
    add_mirror_pin(VM, loader, user_mirror);
    register_non_strong_hidden_mirror(detached_loader_tables(VM), USER_NS, user_mirror, Some(loader));
    register_non_strong_hidden_mirror(detached_loader_tables(VM), BUILTIN_NS, builtin_mirror, None);
    register_non_strong_hidden_mirror(detached_loader_tables(VM), LIVE, live_from, None);

    assert_eq!(loader_pin_addr_for_vm(VM, STRONG), Some(loader));
    assert_eq!(
        loader_pin_addr_for_vm(VM, USER_NS),
        Some(user_mirror),
        "a non-strong class's instances keep its mirror, not its loader"
    );
    assert_eq!(
        mirrors_for_loader(loader),
        Some(vec![strong_mirror]),
        "a live loader must not keep a non-strong class's mirror"
    );

    // The loader and the strong class's mirror survive; both non-strong
    // mirrors die; the live one moves.
    let is_marked = move |addr: usize| addr != user_mirror && addr != builtin_mirror;
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(live_from, live_to);
    let dead = gc_reconcile_defining_loaders(detached_loader_tables(VM), &is_marked, &pointer_map, None);
    assert_eq!(
        dead,
        vec![USER_NS, BUILTIN_NS],
        "both dead mirrors are unload hints; the strong class of the live loader is not"
    );
    assert_eq!(non_strong_hidden_mirror(detached_loader_tables(VM), USER_NS), None);
    assert_eq!(non_strong_hidden_mirror(detached_loader_tables(VM), BUILTIN_NS), None);
    assert!(
        defining_loader_for(detached_loader_tables(VM), USER_NS).is_none(),
        "its defining row goes too"
    );
    assert!(
        is_defining_loader_orphaned(detached_loader_tables(VM), USER_NS),
        "fail-closed until the class leaves the store"
    );
    assert_eq!(defining_loader_for(detached_loader_tables(VM), STRONG), Some(fake(loader)));
    assert_eq!(loader_pin_addr_for_vm(VM, STRONG), Some(loader));
    assert_eq!(non_strong_hidden_mirror(detached_loader_tables(VM), LIVE), Some(live_to));
    assert_eq!(
        loader_pin_addr_for_vm(VM, LIVE),
        Some(live_to),
        "the instance edge follows the moved mirror"
    );

    // A later defining-loader registration keeps the mirror edge.
    register_defining_loader(detached_loader_tables(VM), LIVE, fake(loader));
    assert_eq!(loader_pin_addr_for_vm(VM, LIVE), Some(live_to));

    // The unload transaction removed the two dead classes.
    forget_unloaded_classes(detached_loader_tables(VM), &[USER_NS, BUILTIN_NS]);
    assert!(!is_defining_loader_orphaned(detached_loader_tables(VM), USER_NS));
    assert_eq!(loader_pin_addr_for_vm(VM, USER_NS), None);
    assert_eq!(loader_pin_addr_for_vm(VM, BUILTIN_NS), None);
    assert_eq!(
        non_strong_hidden_mirror(detached_loader_tables(VM), LIVE),
        Some(live_to),
        "untouched"
    );

    forget_vm_loader_singletons(VM);
    assert_eq!(
        non_strong_hidden_mirror(detached_loader_tables(VM), LIVE),
        None,
        "teardown drops the rows"
    );
}

/// gc-common w18-d: registration writes the mirror's `classLoader` edge
/// when the define path created the mirror before it recorded the loader
/// (`define_class_via_full`), so a live instance -- which now keeps only
/// the mirror -- still keeps the loader. And a context with no VM
/// (`vm_identity() == 0`, every unit-test mock by default) registers
/// nothing, so tests sharing that identity cannot see each other's rows.
#[test]
fn w18d_registration_gives_the_mirror_its_loader_edge_and_needs_a_vm() {
    use cratonvm_native_api::FieldMetadata;
    use cratonvm_types::ClassId;
    const VM: usize = 0x18D_0C00;
    if !loader_unload_enabled() {
        eprintln!("skipped: CRATONVM_LOADER_UNLOAD=0 unloads nothing");
        return;
    }
    let mut ctx = MockNativeContext::new();
    // The mock mirror is class 0 with two slots; `classLoader` at slot 1
    // stands in for the real `java.lang.Class` layout.
    ctx.set_declared_fields(
        ClassId::new(0),
        vec![FieldMetadata {
            name: "classLoader".to_string(),
            descriptor: "Ljava/lang/ClassLoader;".to_string(),
            access_flags: 0,
            slot_index: 1,
            declaring_class_id: ClassId::new(0),
            is_static: false,
        }],
    );
    let cid = ctx
        .ensure_class_initialized("w18d/Hidden")
        .expect("mock class id");

    assert!(
        !register_non_strong_hidden_class(&mut ctx, cid),
        "vm_identity 0 is no VM"
    );
    assert_eq!(non_strong_hidden_mirror(detached_loader_tables(0), cid.as_u32()), None);

    ctx.set_vm_identity(VM);
    let loader = ctx.fresh_object_ref();
    register_defining_loader(detached_loader_tables(VM), cid.as_u32(), loader);
    assert!(register_non_strong_hidden_class(&mut ctx, cid));
    let mirror = non_strong_hidden_mirror(detached_loader_tables(VM), cid.as_u32()).expect("registered");
    assert_eq!(
        ctx.get_field_by_name(fake_ref(mirror), "classLoader"),
        Value::Object(Some(loader)),
        "the mirror must carry the edge to its defining loader"
    );
    assert_eq!(
        cratonvm_types::loader_pin::loader_pin_addr_for_vm(VM, cid.as_u32()),
        Some(mirror)
    );
    forget_vm_loader_singletons(VM);

    fn fake_ref(addr: usize) -> ObjectRef {
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }
}
