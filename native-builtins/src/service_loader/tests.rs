use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn file_url_path_preserves_windows_drive_letter() {
    assert_eq!(
        file_url_path_to_fs_path(
            "file:/C:/craton/CratonVM/hibernate-core/META-INF/services/example.Service"
        ),
        Some("C:/craton/CratonVM/hibernate-core/META-INF/services/example.Service".to_string())
    );
    assert_eq!(
        file_url_path_to_fs_path("file:/tmp/service%20descriptor"),
        Some("/tmp/service descriptor".to_string())
    );
}

#[test]
fn provider_name_validation_accepts_fqn() {
    assert!(is_valid_provider_name("com.acme.Foo"));
    assert!(is_valid_provider_name("a.b$Inner"));
    assert!(is_valid_provider_name("Foo_1"));
}

#[test]
fn provider_name_validation_rejects_junk() {
    assert!(!is_valid_provider_name(""));
    assert!(!is_valid_provider_name("foo bar"));
    assert!(!is_valid_provider_name("foo;bar"));
    assert!(!is_valid_provider_name("!foo"));
}

#[test]
fn comment_stripping_drops_hash_suffix() {
    let raw = "com.acme.Foo  # comment";
    let token = raw.split('#').next().unwrap().trim();
    assert_eq!(token, "com.acme.Foo");
}

#[test]
fn embedded_x_content_dependencies_use_the_x_content_archive() {
    assert_eq!(
        derive_impl_jar_module_names(
            "com.fasterxml.jackson.core.util.JsonRecyclerPools$ThreadLocalPool"
        ),
        vec!["x-content"]
    );
    assert_eq!(
        derive_impl_jar_module_names("org.yaml.snakeyaml.Yaml"),
        vec!["x-content"]
    );
}

/// WP1.8-narrow: the descriptor parser strips comments and blanks,
/// rejects malformed lines, and returns valid FQNs unchanged. This
/// is the load-bearing parser the iterator now uses (replacing the
/// JDK BufferedReader chain that the open-sourced revision can't
/// drive end-to-end).
#[test]
fn parse_provider_lines_handles_comments_blanks_and_junk() {
    let body = b"# header comment\n  com.acme.A  \n\ncom.acme.B # trailing\nfoo bar\n!bad\nGood$Inner\n";
    let mut out = Vec::new();
    parse_provider_lines(body, &mut out);
    assert_eq!(out, vec!["com.acme.A", "com.acme.B", "Good$Inner"]);
}

/// WP1.8-narrow: invalid UTF-8 bytes do not panic the parser; they
/// are silently dropped. ServiceLoader descriptors are spec'd as
/// UTF-8 but a malformed JAR shouldn't kill discovery for the rest
/// of the classpath.
#[test]
fn parse_provider_lines_silently_drops_non_utf8() {
    let body: &[u8] = &[0xff, 0xfe, b'\n'];
    let mut out = Vec::new();
    parse_provider_lines(body, &mut out);
    assert!(out.is_empty());
}

#[test]
fn read_jar_url_entry_reads_exact_jar_descriptor() {
    let tmp = tempfile::tempdir().unwrap();
    let jar_path = tmp.path().join("svc.jar");
    let file = std::fs::File::create(&jar_path).unwrap();
    let mut zw = zip::ZipWriter::new(file);
    zw.start_file(
        "META-INF/services/com.acme.Service",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    use std::io::Write;
    zw.write_all(b"com.acme.Provider\n").unwrap();
    zw.finish().unwrap();

    let raw_path = jar_path.to_string_lossy();
    let synthetic_linux_url = format!(
        "jar:file://{}!/META-INF/services/com.acme.Service",
        raw_path.trim_start_matches('/')
    );
    let bytes = read_jar_url_entry(&synthetic_linux_url).unwrap();
    assert_eq!(bytes, b"com.acme.Provider\n");
}
/// Build a `ModifiedClassPathClassLoader`-shaped receiver: a
/// `URLClassLoader` SUBCLASS (so it is not a builtin loader) whose own URL
/// list is a single directory — recorded, and deliberately not holding the
/// service descriptor.
fn modified_classpath_loader(
    ctx: &mut crate::test_utils::MockNativeContext,
    own_dir: &std::path::Path,
) -> cratonvm_types::ObjectRef {
    let url_cid = ctx
        .ensure_class_initialized("java/net/URLClassLoader")
        .expect("URLClassLoader class");
    let mcpcl_cid = ctx
        .ensure_class_initialized(
            "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
        )
        .expect("ModifiedClassPathClassLoader class");
    ctx.set_superclass(mcpcl_cid, url_cid);
    // The mock resolves field names off an exact-class-name table, so a
    // SUBCLASS of URLClassLoader needs its inherited `ucp` declared or
    // `set_field_by_name` silently no-ops and the fixture would present a
    // loader with no URLs at all.
    ctx.set_declared_fields(
        mcpcl_cid,
        vec![cratonvm_native_api::FieldMetadata {
            name: "ucp".to_string(),
            descriptor: "Ljdk/internal/loader/URLClassPath;".to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: mcpcl_cid,
            is_static: false,
        }],
    );
    let mut new_ref = |ctx: &mut crate::test_utils::MockNativeContext, name: &str| match ctx
        .new_object(name)
        .unwrap()
    {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected {name} object, got {other:?}"),
    };
    let loader = new_ref(
        ctx,
        "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
    );
    let ucp = new_ref(ctx, "jdk/internal/loader/URLClassPath");
    let url = new_ref(ctx, "java/net/URL");
    let path = ctx.create_string(&own_dir.to_string_lossy());
    ctx.set_field(url, 3, Value::Object(Some(path)));
    ctx.set_field_by_name(loader, "ucp", Value::Object(Some(ucp)));
    let urls = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(urls, 0, Value::Object(Some(url)));
    ctx.set_field(
        ucp,
        crate::classloader::UCP_STASHED_URLS,
        Value::Object(Some(urls)),
    );
    loader
}

fn service_loader_for(
    ctx: &mut crate::test_utils::MockNativeContext,
    service: &str,
    loader: Value,
) -> cratonvm_types::ObjectRef {
    let service_id = ctx
        .ensure_class_initialized(service)
        .expect("create service class");
    let service_mirror = ctx.get_class_mirror(service_id);
    match build_service_loader(ctx, Value::Object(Some(service_mirror)), loader)
        .expect("build ServiceLoader")
    {
        Some(Value::Object(Some(sl))) => sl,
        other => panic!("expected ServiceLoader object, got {other:?}"),
    }
}

/// The flat classpath scan must not re-add a provider registration that the
/// receiver's own (exclusion-filtered) URL list does not carry.
///
/// This is the `@ClassPathExclusions` leak: the descriptor came back from a
/// process-wide scan, `loadClass` then correctly refused the class it named,
/// and `ServiceLoader` raised `ServiceConfigurationError: ... Provider ...
/// not found` where HotSpot discovers no providers at all.
#[test]
fn discover_providers_skips_flat_scan_for_a_loader_with_its_own_urls() {
    const SVC: &str = "org.slf4j.spi.SLF4JServiceProvider";
    let resource = format!("META-INF/services/{SVC}");
    let dir = tempfile::tempdir().expect("tempdir");

    let mut ctx = mock_ctx();
    // The process-wide classpath DOES carry the descriptor — without this
    // the assertion below could not fail even if the scan still ran.
    ctx.set_resource(
        &resource,
        b"ch.qos.logback.classic.spi.LogbackServiceProvider\n".to_vec(),
    );
    // `logback-classic.jar` also DECLARES this provider in its `module-info`.
    // On a real JVM that declaration is invisible to a class-path loader; the
    // VM-global module registry offered it to every caller, so the same
    // provider leaked back through a second door after the flat scan closed.
    ctx.set_module_providers(
        "org/slf4j/spi/SLF4JServiceProvider",
        vec!["ch/qos/logback/classic/spi/LogbackServiceProvider"],
    );

    let loader = modified_classpath_loader(&mut ctx, dir.path());
    assert!(
        crate::classloader::object_extends(&ctx, loader, "java/net/URLClassLoader"),
        "fixture loader must extend URLClassLoader"
    );
    assert!(
        crate::classloader::loader_owns_complete_resource_view(&ctx, loader),
        "fixture must present a URLClassLoader-family loader with recorded URLs"
    );
    let sl = service_loader_for(
        &mut ctx,
        "org/slf4j/spi/SLF4JServiceProvider",
        Value::Object(Some(loader)),
    );
    // `discover_providers` reads the named `loader` field, then legacy slot 1.
    assert!(
        matches!(ctx.get_field_by_name(sl, "loader"), Value::Object(Some(_)))
            || matches!(ctx.get_field(sl, 1), Value::Object(Some(_))),
        "fixture must put the loader where discover_providers reads it"
    );

    let providers = discover_providers(&mut ctx, sl).expect("discover providers");
    assert!(
        providers.is_empty(),
        "a loader whose own URL list excludes the jar must discover no \
         providers from it; got {providers:?}"
    );
}

/// Control for the test above: with no loader to answer for itself, the flat
/// classpath scan is still the discovery mechanism.
#[test]
fn discover_providers_keeps_flat_scan_without_a_scoped_loader() {
    const SVC: &str = "com.acme.Service";
    let resource = format!("META-INF/services/{SVC}");

    let mut ctx = mock_ctx();
    ctx.set_resource(&resource, b"com.acme.Provider\n".to_vec());
    let sl = service_loader_for(&mut ctx, "com/acme/Service", Value::Object(None));

    let providers = discover_providers(&mut ctx, sl).expect("discover providers");
    assert_eq!(
        providers,
        vec!["com.acme.Provider"],
        "the flat classpath scan must still serve loaders CratonVM has no URL view of"
    );
}

#[test]
fn discover_providers_includes_jpms_module_provides_entries() {
    let mut ctx = mock_ctx();
    ctx.set_module_providers(
        "org/apache/lucene/codecs/Codec",
        vec!["org/apache/lucene/codecs/lucene104/Lucene104Codec"],
    );

    let service_id = ctx
        .ensure_class_initialized("org/apache/lucene/codecs/Codec")
        .expect("create service class");
    let service_mirror = ctx.get_class_mirror(service_id);
    let sl = match build_service_loader(
        &mut ctx,
        Value::Object(Some(service_mirror)),
        Value::Object(None),
    )
    .expect("build ServiceLoader")
    {
        Some(Value::Object(Some(sl))) => sl,
        other => panic!("expected ServiceLoader object, got {other:?}"),
    };

    let providers = discover_providers(&mut ctx, sl).expect("discover providers");
    assert_eq!(
        providers,
        vec!["org.apache.lucene.codecs.lucene104.Lucene104Codec"],
        "module-info `provides Codec with Lucene104Codec` must participate in ServiceLoader discovery",
    );
}
