use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};

/// Build a synthetic java/io/ByteArrayInputStream holding the given
/// bytes. Mirrors the 4-field layout (buf=0, pos=1, mark=2, count=3)
/// that the p59 fast path in `p59_read_input_stream_fully` reads.
fn make_bais(ctx: &mut crate::test_utils::MockNativeContext, data: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, data.len());
    for (i, b) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    let cid = ctx
        .ensure_class_initialized("java/io/ByteArrayInputStream")
        .expect("register BAIS class");
    let bais = ctx.alloc_object(cid, 4);
    ctx.set_field(bais, 0, Value::Object(Some(arr)));
    ctx.set_field(bais, 1, Value::Int(0));
    ctx.set_field(bais, 2, Value::Int(0));
    ctx.set_field(bais, 3, Value::Int(data.len() as i32));
    bais
}

/// Drive the Manifest(InputStream) native against a given byte buffer.
/// Returns the constructed Manifest object.
fn call_manifest_init(
    reg: &NativeMethodRegistry,
    ctx: &mut crate::test_utils::MockNativeContext,
    bytes: &[u8],
) -> ObjectRef {
    let bais = make_bais(ctx, bytes);
    let mf_cid = ctx
        .ensure_class_initialized("java/util/jar/Manifest")
        .expect("register Manifest class");
    let mf = ctx.alloc_object(mf_cid, 2);
    let init = reg
        .find(
            "java/util/jar/Manifest",
            "<init>",
            "(Ljava/io/InputStream;)V",
        )
        .expect("Manifest.<init>(InputStream) must be registered");
    init(ctx, &[Value::Object(Some(mf)), Value::Object(Some(bais))])
        .expect("Manifest.<init>(InputStream) ok");
    mf
}

/// Helper: resolve Attributes.getValue(String) on an Attributes object.
fn get_value(
    reg: &NativeMethodRegistry,
    ctx: &mut crate::test_utils::MockNativeContext,
    attrs: ObjectRef,
    key: &str,
) -> Option<String> {
    let get = reg
        .find(
            "java/util/jar/Attributes",
            "getValue",
            "(Ljava/lang/String;)Ljava/lang/String;",
        )
        .expect("Attributes.getValue must be registered");
    let k = ctx.create_string(key);
    let out =
        get(ctx, &[Value::Object(Some(attrs)), Value::Object(Some(k))]).expect("getValue ok");
    match out {
        Some(Value::Object(Some(s))) => ctx.read_string(s),
        _ => None,
    }
}

#[test]
fn t10_manifest_parser_preserves_folded_main_attribute() {
    // `JarFile.getManifest()` and `Manifest(InputStream)` deliberately
    // share this parser. This is the long-Class-Path shape Spring Boot
    // writes when it emits a launcher manifest.
    let parsed = p59_parse_manifest_bytes(
        b"Manifest-Version: 1.0\r\nClass-Path: lib/one.jar lib/two.jar \r\n lib/three.jar\r\n\r\n",
    )
    .expect("valid folded manifest");
    assert_eq!(
        parsed.main,
        vec![
            ("Manifest-Version".to_string(), "1.0".to_string()),
            (
                "Class-Path".to_string(),
                "lib/one.jar lib/two.jar lib/three.jar".to_string(),
            ),
        ]
    );
}

// STALE (unit-test mock cannot exercise this path): the Manifest parser now
// populates a REAL `java.util.jar.Attributes` via `ctx.invoke(putValue)` and
// reads it via real `getValue`/`size` bytecode (those are no longer natives).
// A `MockNativeContext` cannot execute bytecode, so these assertions can't
// pass here — the coverage belongs in an integration test on a real VM.
#[test]
#[ignore = "needs real VM: Manifest/Attributes now run real bytecode, not natives"]
fn t10_manifest_init_from_input_stream_parses_main_attributes() {
    let mut reg = NativeMethodRegistry::new();
    register_p59_jar(&mut reg);
    let mut ctx = mock_ctx();

    // Manifest with three main attributes (CRLF line endings, as real
    // MANIFEST.MF files use), no per-entry sections.
    let mf_bytes =
        b"Manifest-Version: 1.0\r\nMain-Class: com.example.Foo\r\nCreated-By: cratonvm\r\n\r\n";
    let mf = call_manifest_init(&reg, &mut ctx, mf_bytes);

    // Drive getMainAttributes() to retrieve the Attributes object.
    let get_main = reg
        .find(
            "java/util/jar/Manifest",
            "getMainAttributes",
            "()Ljava/util/jar/Attributes;",
        )
        .expect("getMainAttributes registered");
    let attrs_val = get_main(&mut ctx, &[Value::Object(Some(mf))])
        .expect("getMainAttributes ok")
        .expect("getMainAttributes returns Some");
    let attrs = match attrs_val {
        Value::Object(Some(a)) => a,
        other => panic!("expected Object, got {other:?}"),
    };

    assert_eq!(
        get_value(&reg, &mut ctx, attrs, "Main-Class"),
        Some("com.example.Foo".to_string()),
        "Main-Class must round-trip through the parser"
    );
    assert_eq!(
        get_value(&reg, &mut ctx, attrs, "Manifest-Version"),
        Some("1.0".to_string())
    );
    assert_eq!(
        get_value(&reg, &mut ctx, attrs, "Created-By"),
        Some("cratonvm".to_string())
    );
    // Case-insensitive key lookup, per manifest spec.
    assert_eq!(
        get_value(&reg, &mut ctx, attrs, "main-class"),
        Some("com.example.Foo".to_string())
    );
    // Unknown key must return null.
    assert_eq!(get_value(&reg, &mut ctx, attrs, "Nonexistent"), None);
}

#[test]
#[ignore = "needs real VM: Manifest/Attributes now run real bytecode, not natives"]
fn t10_manifest_init_from_input_stream_handles_empty_stream() {
    let mut reg = NativeMethodRegistry::new();
    register_p59_jar(&mut reg);
    let mut ctx = mock_ctx();

    // Empty stream — parser must produce a Manifest with empty main
    // attributes and empty entries, not throw.
    let mf = call_manifest_init(&reg, &mut ctx, b"");

    let get_main = reg
        .find(
            "java/util/jar/Manifest",
            "getMainAttributes",
            "()Ljava/util/jar/Attributes;",
        )
        .unwrap();
    let attrs = match get_main(&mut ctx, &[Value::Object(Some(mf))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(a)) => a,
        other => panic!("expected Object, got {other:?}"),
    };
    let size = reg.find("java/util/jar/Attributes", "size", "()I").unwrap();
    let n = match size(&mut ctx, &[Value::Object(Some(attrs))])
        .unwrap()
        .unwrap()
    {
        Value::Int(v) => v,
        other => panic!("expected Int, got {other:?}"),
    };
    assert_eq!(n, 0, "empty stream must yield zero main attributes");

    let get_entries = reg
        .find("java/util/jar/Manifest", "getEntries", "()Ljava/util/Map;")
        .unwrap();
    let entries = match get_entries(&mut ctx, &[Value::Object(Some(mf))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(m)) => m,
        other => panic!("expected Object, got {other:?}"),
    };
    let entries_size = match ctx.get_field(entries, 1) {
        Value::Int(v) => v,
        other => panic!("expected Int size, got {other:?}"),
    };
    assert_eq!(entries_size, 0, "empty stream must yield zero entries");
}

#[test]
#[ignore = "needs real VM: Manifest/Attributes now run real bytecode, not natives"]
fn t10_manifest_init_from_input_stream_handles_entries_and_continuations() {
    let mut reg = NativeMethodRegistry::new();
    register_p59_jar(&mut reg);
    let mut ctx = mock_ctx();

    // Manifest with a continuation line in the main section, plus one
    // per-entry section. Mixed CRLF + LF on purpose.
    let mf_bytes = b"Manifest-Version: 1.0\r\nMain-Class: com.example\r\n .Very.Long.Class.Name\r\n\r\nName: pkg/Foo.class\r\nSHA-256-Digest: abc123\r\n\r\n";
    let mf = call_manifest_init(&reg, &mut ctx, mf_bytes);

    // Main attributes: continuation line must be appended.
    let get_main = reg
        .find(
            "java/util/jar/Manifest",
            "getMainAttributes",
            "()Ljava/util/jar/Attributes;",
        )
        .unwrap();
    let attrs = match get_main(&mut ctx, &[Value::Object(Some(mf))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(a)) => a,
        _ => panic!(),
    };
    assert_eq!(
        get_value(&reg, &mut ctx, attrs, "Main-Class"),
        Some("com.example.Very.Long.Class.Name".to_string()),
        "continuation line must be appended to previous value"
    );

    // Entry section must materialise one entry whose Attributes
    // carry SHA-256-Digest = abc123.
    let get_entries = reg
        .find("java/util/jar/Manifest", "getEntries", "()Ljava/util/Map;")
        .unwrap();
    let entries = match get_entries(&mut ctx, &[Value::Object(Some(mf))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(m)) => m,
        _ => panic!(),
    };
    // Entries layout: buckets=field0, size=field1. One entry => size=1.
    assert_eq!(ctx.get_field(entries, 1), Value::Int(1));
    let buckets = match ctx.get_field(entries, 0) {
        Value::Object(Some(b)) => b,
        _ => panic!("entries missing buckets"),
    };
    let entry_name = match ctx.get_array_element(buckets, 0) {
        Value::Object(Some(k)) => ctx.read_string(k),
        _ => None,
    };
    assert_eq!(entry_name.as_deref(), Some("pkg/Foo.class"));
    let entry_attrs = match ctx.get_array_element(buckets, 1) {
        Value::Object(Some(a)) => a,
        _ => panic!("entry value must be Attributes"),
    };
    assert_eq!(
        get_value(&reg, &mut ctx, entry_attrs, "SHA-256-Digest"),
        Some("abc123".to_string())
    );
}

#[test]
#[ignore = "needs real VM: Manifest/Attributes now run real bytecode, not natives"]
fn t10_jar_file_get_manifest_roundtrip() {
    use std::io::Write;

    // Build a minimal JAR on disk with a real MANIFEST.MF so the
    // synthetic `p59_jar_file_init` can open it via the zip crate
    // and the `getValue("Main-Class")` lookup now hits the updated
    // Attributes.getValue native (previously a no-op stub that
    // returned null even when the manifest was parsed).
    let tmp_dir = std::env::temp_dir().join("cratonvm_t10_manifest_roundtrip");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let jar_path = tmp_dir.join("t10.jar");
    {
        let file = std::fs::File::create(&jar_path).expect("create jar");
        let mut zw = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zw.write_all(b"Manifest-Version: 1.0\r\nMain-Class: com.example.Bar\r\n\r\n")
            .unwrap();
        zw.finish().unwrap();
    }

    let mut reg = NativeMethodRegistry::new();
    register_p59_jar(&mut reg);
    let mut ctx = mock_ctx();

    // Open the JAR via JarFile(String) — the synthetic init reads and
    // parses the manifest from disk using the zip crate.
    let jf_cid = ctx
        .ensure_class_initialized("java/util/jar/JarFile")
        .unwrap();
    let jf = ctx.alloc_object(jf_cid, 2);
    let path_str = ctx.create_string(jar_path.to_string_lossy().as_ref());
    let init = reg
        .find("java/util/jar/JarFile", "<init>", "(Ljava/lang/String;)V")
        .expect("JarFile.<init>(String) registered");
    init(
        &mut ctx,
        &[Value::Object(Some(jf)), Value::Object(Some(path_str))],
    )
    .expect("JarFile.<init> ok");

    // getManifest() — must return a non-null Manifest.
    let get_mf = reg
        .find(
            "java/util/jar/JarFile",
            "getManifest",
            "()Ljava/util/jar/Manifest;",
        )
        .unwrap();
    let mf = match get_mf(&mut ctx, &[Value::Object(Some(jf))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(m)) => m,
        other => panic!("getManifest returned {other:?}"),
    };

    // getMainAttributes().getValue("Main-Class") — this is the
    // regression point: before the Attributes.getValue fix this
    // returned null even though p98_read_jar_manifest had parsed
    // the value into the buckets array.
    let get_main = reg
        .find(
            "java/util/jar/Manifest",
            "getMainAttributes",
            "()Ljava/util/jar/Attributes;",
        )
        .unwrap();
    let attrs = match get_main(&mut ctx, &[Value::Object(Some(mf))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(a)) => a,
        other => panic!("getMainAttributes returned {other:?}"),
    };
    assert_eq!(
        get_value(&reg, &mut ctx, attrs, "Main-Class"),
        Some("com.example.Bar".to_string()),
        "Main-Class attribute must round-trip from JAR on disk"
    );

    let _ = std::fs::remove_file(&jar_path);
}
