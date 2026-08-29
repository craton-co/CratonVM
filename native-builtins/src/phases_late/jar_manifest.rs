// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.jar` natives: JarFile/JarEntry/Manifest/Attributes and the Spring Boot fat-jar launcher support.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};

// =============================================================================
// java.util.jar — JarFile, JarEntry, Manifest, Attributes
// JarFile = 2-field synthetic (path=0 String, manifest=1 Manifest)
// Manifest = 2-field synthetic (mainAttrs=0 HashMap, entries=1 HashMap)
// =============================================================================

pub fn register_p59_jar(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let jf = "java/util/jar/JarFile";
    r.register(jf, "<init>", "(Ljava/lang/String;)V", p59_jar_file_init);
    r.register(jf, "<init>", "(Ljava/lang/String;Z)V", p59_jar_file_init);
    r.register(jf, "<init>", "(Ljava/io/File;)V", p59_jar_file_init_file);
    // File-first overloads. `p59_jar_file_init_file` only reads args[1] (the
    // File) and ignores the rest, so the verify/mode/Runtime.Version variants
    // route through it unchanged. The 4-arg multi-release constructor
    // `JarFile(File, boolean, int, Runtime.Version)` is the one Tomcat's
    // AbstractArchiveResourceSet.openJarFile uses; without this it fell through
    // to real ZipFile bytecode and failed with "ZipException: zip file is empty"
    // (whole catalina.webresources JAR cluster).
    r.register(jf, "<init>", "(Ljava/io/File;Z)V", p59_jar_file_init_file);
    r.register(jf, "<init>", "(Ljava/io/File;ZI)V", p59_jar_file_init_file);
    r.register(
        jf,
        "<init>",
        "(Ljava/io/File;ZILjava/lang/Runtime$Version;)V",
        p59_jar_file_init_file,
    );
    r.register(
        jf,
        "getManifest",
        "()Ljava/util/jar/Manifest;",
        p59_jar_file_manifest,
    );
    r.register(
        jf,
        "getManifestFromReference",
        "()Ljava/util/jar/Manifest;",
        p59_jar_file_manifest,
    );
    r.register(
        jf,
        "getEntry",
        "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let entry_name = if let Some(Value::Object(Some(s))) = args.get(1) {
                ctx.read_string(*s).unwrap_or_default()
            } else {
                return Ok(Some(Value::Object(None)));
            };
            // Get the jar file path and check if entry exists
            let path = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            // A JarEntry is a ZipEntry, so use the single materializer for
            // both APIs. This keeps central-directory comments and the
            // real-JDK field layout identical for getEntry/getJarEntry.
            let resolved = p59_jar_lookup_versioned_entry(ctx, this, &path, &entry_name)?;
            if !matches!(resolved, Some(Value::Object(None))) {
                return Ok(resolved);
            }
            // Cached parse (O(1) per call; avoids re-parsing the central
            // directory on every lookup — see `jar_contents_cached`).
            if let Some(contents) = jar_contents_cached(&path) {
                if let Some(rec) = contents.by_name.get(&entry_name) {
                    let size = rec.size;
                    let csize = rec.csize;
                    let method = rec.method;
                    let crc = rec.crc;
                    let ze = try_alloc_concurrent_synthetic(ctx, "java/util/jar/JarEntry", 4)?;
                    // Pin across the create_string below — a moving young GC
                    // there would relocate the fresh entry (native stale-local
                    // family).
                    let ze_pin = ctx.pin_native_root(ze);
                    let name_s = ctx.create_string(&entry_name);
                    let ze = ctx.read_native_pin(ze_pin, ze);
                    ctx.unpin_native_roots(ze_pin);
                    ctx.set_field(ze, 0, Value::Object(Some(name_s)));
                    ctx.set_field(ze, 1, Value::Long(size));
                    ctx.set_field(ze, 2, Value::Long(csize));
                    ctx.set_field(ze, 3, Value::Int(method));
                    // Real-JDK mode: `ZipEntry.getSize()/getMethod()/...` run
                    // real bytecode that reads the REAL fields by their actual
                    // offset, NOT the synthetic slots above. Quarkus'
                    // RunnerClassLoader sizes its class-byte read from
                    // `entry.getSize()`; if that returns 0 the class is defined
                    // from a 0-length array → ClassFormatError. Mirror the real
                    // field names (matches zip_real_jar::alloc_zip_entry).
                    ctx.set_field_by_name(ze, "name", Value::Object(Some(name_s)));
                    ctx.set_field_by_name(ze, "size", Value::Long(size));
                    ctx.set_field_by_name(ze, "csize", Value::Long(csize));
                    ctx.set_field_by_name(ze, "method", Value::Int(method));
                    ctx.set_field_by_name(ze, "crc", Value::Long(crc));
                    return Ok(Some(Value::Object(Some(ze))));
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );
    // getInputStream(ZipEntry) — the synthetic JarFile has no real `jzfile`
    // handle, so the real ZipFile.getInputStream returns null (then Tomcat's
    // JarInputStreamWrapper.close NPEs on the null stream). Read the entry's
    // bytes from the JAR (path in field 0) by entry name via the zip crate and
    // hand back a ByteArrayInputStream. (catalina.webresources JAR resources.)
    r.register(
        jf,
        "getInputStream",
        "(Ljava/util/zip/ZipEntry;)Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let entry_name = match args.get(1) {
                Some(Value::Object(Some(e)))
                    if ctx
                        .class_name_arc_of_id(ctx.class_id_of_object(*e))
                        .as_deref()
                        == Some(
                            "org/springframework/boot/loader/jar/NestedJarFile$NestedJarEntry",
                        ) =>
                {
                    let content_entry = match ctx.get_field_by_name(*e, "contentEntry") {
                        Value::Object(Some(content_entry)) => content_entry,
                        _ => return Ok(Some(Value::Object(None))),
                    };
                    match ctx.invoke_virtual(
                        content_entry,
                        "getName",
                        "()Ljava/lang/String;",
                        &[],
                    )? {
                        Some(Value::Object(Some(name))) => {
                            ctx.read_string(name).unwrap_or_default()
                        }
                        _ => return Ok(Some(Value::Object(None))),
                    }
                }
                Some(Value::Object(Some(e))) => match ctx.get_field_by_name(*e, "name") {
                    // A Spring Boot NestedJarEntry keeps the physical entry
                    // name in ZipEntry.name while overriding getName() with
                    // the logical multi-release name. The backing JAR must
                    // be read by that physical name. Compact native entries
                    // have no named layout, so retain the slot-0 fallback.
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => match ctx.get_field(*e, 0) {
                        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                        _ => return Ok(Some(Value::Object(None))),
                    },
                },
                _ => return Ok(Some(Value::Object(None))),
            };
            // Cached, lazy decompress (O(1) per call after the first read of
            // this entry; avoids both re-parsing the whole central directory
            // on every entry AND decompressing entries nothing ever reads —
            // see `jar_entry_bytes_cached`).
            let bytes: Option<std::sync::Arc<Vec<u8>>> = jar_entry_bytes_cached(&path, &entry_name);
            let bytes = match bytes {
                Some(b) => b,
                None => return Ok(Some(Value::Object(None))),
            };
            // ByteArrayInputStream: buf(0), pos(1), mark(2), count(3)
            let bais = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
            // Pin across the array alloc below — a moving young GC there would
            // relocate the fresh stream (native stale-local family).
            let bais_pin = ctx.pin_native_root(bais);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            let bais = ctx.read_native_pin(bais_pin, bais);
            ctx.unpin_native_roots(bais_pin);
            // Keep archive traversal linear in the archive bytes.  In
            // particular, the signed-JAR parity test drains every bcprov
            // entry; per-element NativeContext calls turn that into millions
            // of VM crossings before ByteArrayInputStream can be returned.
            ctx.write_byte_array_from(arr, 0, bytes.as_slice());
            ctx.set_field(bais, 0, Value::Object(Some(arr)));
            ctx.set_field(bais, 1, Value::Int(0));
            ctx.set_field(bais, 2, Value::Int(0));
            ctx.set_field(bais, 3, Value::Int(bytes.len() as i32));
            Ok(Some(Value::Object(Some(bais))))
        },
    );
    r.register(
        jf,
        "getJarEntry",
        "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
        |ctx, args| {
            // Reuse the central-directory lookup so callers that walk via
            // getJarEntry (e.g. JarFileArchive.getNestedJarUrl ->
            // JarUrl.create) see a populated synthetic JarEntry instead of
            // null.
            let this = obj_arg(args, 0)?;
            // Virtual dispatch reaches this inherited JarFile bridge before
            // NestedJarFile's bytecode override. Delegate from the bridge
            // itself so the concrete receiver keeps its multi-release
            // `NestedJarEntry` representation.
            if ctx.class_name_arc_of_id(ctx.class_id_of_object(this)).as_deref()
                == Some("org/springframework/boot/loader/jar/NestedJarFile")
            {
                return ctx.invoke(
                    "org/springframework/boot/loader/jar/NestedJarFile",
                    "getNestedJarEntry",
                    "(Ljava/lang/String;)Lorg/springframework/boot/loader/jar/NestedJarFile$NestedJarEntry;",
                    args,
                );
            }
            let entry_name = if let Some(Value::Object(Some(s))) = args.get(1) {
                ctx.read_string(*s).unwrap_or_default()
            } else {
                return Ok(Some(Value::Object(None)));
            };
            let path = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            p59_jar_lookup_versioned_entry(ctx, this, &path, &entry_name)
        },
    );
    r.register(jf, "close", "()V", |ctx, args| {
        // JarFile is 2-field (path=0, manifest=1). Mark closed by clearing the path field.
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, Value::Object(None));
        }
        Ok(None)
    });
    r.register(jf, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    // `register_p59_jar` runs after native-io's JarFile bridge and therefore
    // owns construction. Keep the inherited ZipFile accessors in this same
    // path: native-io's `size`/`getComment` use its side-table handle, while
    // p59 construction stores the backing path directly on the object.
    r.register(jf, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => return Ok(Some(Value::Int(0))),
        };
        let size = jar_contents_cached(&path)
            .map(|contents| contents.order.len().min(i32::MAX as usize) as i32)
            .unwrap_or(0);
        Ok(Some(Value::Int(size)))
    });
    r.register(jf, "getComment", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            // `close()` clears the path slot for the synthetic JarFile. The
            // inherited ZipFile contract is to reject every accessor after
            // close, rather than silently treating the archive as commentless.
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "zip file closed".into(),
                }
                .into())
            }
        };
        let comment = std::fs::File::open(path)
            .ok()
            .and_then(|file| zip::ZipArchive::new(file).ok())
            .and_then(|archive| {
                (!archive.comment().is_empty())
                    .then(|| String::from_utf8_lossy(archive.comment()).into_owned())
            });
        Ok(Some(match comment {
            Some(comment) => Value::Object(Some(ctx.create_string(&comment))),
            None => Value::Object(None),
        }))
    });
    // `ManifestInfo.isMultiRelease()` uses Attributes.containsKey(Name).
    // Attributes' map is initialized by the existing side-table-aware native;
    // calling the real bytecode here can instead observe an incomplete JDK
    // HashMap layout and falsely report that `Multi-Release` is absent.
    r.register(
        "java/util/jar/Attributes",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, 0) {
                Value::Object(Some(map)) => map,
                _ => return Ok(Some(Value::Int(0))),
            };
            cratonvm_native_collections::native_map_contains_key_pub(
                ctx,
                &[Value::Object(Some(map)), key],
            )
        },
    );
    // Spring Boot's NestedJarEntry lazily copies optional central-directory
    // comments into its ZipEntry superclass. JDK 25's bytecode dereferences a
    // null comment during CEN validation in our compact/native setup; the JVM
    // contract permits a null comment, so store it directly in the real layout.
    r.register(
        "java/util/zip/ZipEntry",
        "setComment",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let comment = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "comment", comment);
            if ctx.object_num_fields(this) <= 5 {
                ctx.set_field(this, 4, comment);
            }
            Ok(None)
        },
    );
    // Spring Boot's NestedJarFile asks its package-private ManifestInfo
    // whether the nested archive is multi-release before resolving versioned
    // entries. The manifest value bridge is reliable, whereas the real JDK
    // Attributes.containsKey path can observe an incompatible map layout in
    // this VM. Preserve the class's exact contract by querying the canonical
    // `Multi-Release` value directly and accepting only `true` (case-free).
    r.register(
        "org/springframework/boot/loader/jar/ManifestInfo",
        "isMultiRelease",
        "()Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let manifest = match ctx.get_field(this, 0) {
                Value::Object(Some(manifest)) => manifest,
                _ => return Ok(Some(Value::Int(0))),
            };
            let attributes = match ctx.invoke(
                "java/util/jar/Manifest",
                "getMainAttributes",
                "()Ljava/util/jar/Attributes;",
                &[Value::Object(Some(manifest))],
            )? {
                Some(Value::Object(Some(attributes))) => attributes,
                _ => return Ok(Some(Value::Int(0))),
            };
            let key = ctx.create_string("Multi-Release");
            let value = ctx.invoke(
                "java/util/jar/Attributes",
                "getValue",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[Value::Object(Some(attributes)), Value::Object(Some(key))],
            )?;
            let is_multi_release = matches!(
                value,
                Some(Value::Object(Some(value)))
                    if ctx.read_string(value).is_some_and(|value| value.eq_ignore_ascii_case("true"))
            );
            Ok(Some(Value::Int(i32::from(is_multi_release))))
        },
    );
    // The generic JarFile bridge owns the inherited public `getJarEntry`
    // slot, but NestedJarFile has richer multi-release entry semantics. Route
    // that one public dispatch to its private implementation rather than
    // materializing a generic JarEntry: NestedJarEntry retains a logical name
    // while its ZipEntry superclass stores the physical versioned name.
    r.register(
        "org/springframework/boot/loader/jar/NestedJarFile",
        "getJarEntry",
        "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
        |ctx, args| {
            ctx.invoke(
                "org/springframework/boot/loader/jar/NestedJarFile",
                "getNestedJarEntry",
                "(Ljava/lang/String;)Lorg/springframework/boot/loader/jar/NestedJarFile$NestedJarEntry;",
                args,
            )
        },
    );
    r.register(
        "org/springframework/boot/loader/jar/NestedJarFile$NestedJarEntry",
        "getRealName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let content_entry = match ctx.get_field_by_name(this, "contentEntry") {
                Value::Object(Some(content_entry)) => content_entry,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.invoke_virtual(content_entry, "getName", "()Ljava/lang/String;", &[])
        },
    );
    // Trigger Spring's verifier so malformed nested archives retain its
    // specified IllegalStateException, then expose the compact JarFile view
    // (which deliberately has no JarVerifier state) on a successful check.
    let nested_entry = "org/springframework/boot/loader/jar/NestedJarFile$NestedJarEntry";
    r.register(
        nested_entry,
        "getCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let _ = ctx.invoke_virtual_bytecode_only(
                this,
                "getCertificates",
                "()[Ljava/security/cert/Certificate;",
                &[],
            )?;
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(
        nested_entry,
        "getCodeSigners",
        "()[Ljava/security/CodeSigner;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let _ = ctx.invoke_virtual_bytecode_only(
                this,
                "getCodeSigners",
                "()[Ljava/security/CodeSigner;",
                &[],
            )?;
            Ok(Some(Value::Object(None)))
        },
    );
    // UrlJarFile overrides getEntry solely to attach its URL-aware manifest
    // wrapper. Its `super.getEntry` must still use Craton's compact JarFile
    // state rather than real ZipFile bytecode (which expects `res.zsrc`).
    r.register(
        "org/springframework/boot/loader/net/protocol/jar/UrlJarFile",
        "getEntry",
        "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let entry = ctx.invoke(
                "java/util/jar/JarFile",
                "getEntry",
                "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
                args,
            )?;
            let manifest = match ctx.get_field_by_name(this, "manifest") {
                Value::Object(Some(manifest)) => manifest,
                _ => return Ok(entry),
            };
            let entry = entry.unwrap_or(Value::Object(None));
            ctx.invoke(
                "org/springframework/boot/loader/net/protocol/jar/UrlJarEntry",
                "of",
                "(Ljava/util/zip/ZipEntry;Lorg/springframework/boot/loader/net/protocol/jar/UrlJarManifest;)Lorg/springframework/boot/loader/net/protocol/jar/UrlJarEntry;",
                &[entry, Value::Object(Some(manifest))],
            )
        },
    );
    // ZipContent's signature-file detector only needs to recognize three
    // fixed ASCII suffixes.  Its Java `<clinit>` builds that tiny table via a
    // Stream.map(...).toList() pipeline; on the real JDK path that first
    // stream initialization can spend minutes in charset/version-regex setup
    // before a signed JAR is even opened.  Keep the detector's observable
    // contract, but make the immutable table implicit and allocation-free.
    let signature_files = "org/springframework/boot/loader/zip/ZipContent$SignatureFiles";
    r.register(signature_files, "<clinit>", "()V", |_ctx, _args| Ok(None));
    r.register(signature_files, "bufferEndsWithSignatureSuffix", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buffer = match ctx.get_field_by_name(this, "buffer") {
            Value::Object(Some(buffer)) => buffer,
            _ => return Ok(Some(Value::Int(0))),
        };
        let array = match ctx.invoke_virtual(buffer, "array", "()[B", &[])? {
            Some(Value::Object(Some(array))) => array,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(array);
        for suffix in [b".DSA".as_slice(), b".RSA".as_slice(), b".EC".as_slice()] {
            if len < suffix.len() {
                continue;
            }
            let start = len - suffix.len();
            if suffix.iter().enumerate().all(|(index, byte)| {
                matches!(ctx.get_array_element(array, start + index), Value::Int(value) if value as u8 == *byte)
            }) {
                return Ok(Some(Value::Int(1)));
            }
        }
        Ok(Some(Value::Int(0)))
    });
    // `JarFile.isMultiRelease()` is true iff the manifest's main section
    // declares `Multi-Release: true` (JEP 238). The old blanket `false` was
    // stale: the compact bridge DOES remap versioned entries — see
    // `p59_jar_lookup_versioned_entry`, which drives `META-INF/versions/N/`
    // resolution off the same `p59_jar_is_multi_release` helper used here.
    // Answer from the manifest so callers agree with the entries we hand back.
    r.register(jf, "isMultiRelease", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = p59_jar_file_path(ctx, this);
        let multi_release = !path.is_empty() && p59_jar_is_multi_release(&path);
        Ok(Some(Value::Int(i32::from(multi_release))))
    });
    // JarFile.stream() — Spring Boot 3 fat-jar launcher
    // (org.springframework.boot.loader.launch.JarFileArchive.getClassPathUrls)
    // walks the JarFile via stream() to enumerate BOOT-INF/lib/*.jar entries
    // for the LaunchedClassLoader URL set. Real-JDK bytecode delegates this
    // through SharedSecrets.JUZFA -> ZipFile.jarStream which dereferences a
    // private `res.zsrc` field that our 2-field synthetic JarFile does not
    // populate, NPE-ing in ZipFile.ensureOpen. Override returns a synthetic
    // 1-field Stream populated with synthetic JarEntry instances built from
    // the central directory via the Rust `zip` crate.
    r.register(
        jf,
        "stream",
        "()Ljava/util/stream/Stream;",
        p59_jar_file_stream,
    );
    // JarFile.entries() — Enumeration<JarEntry>. Same backing data as
    // stream(); used by classpath scanners that prefer the legacy iteration.
    r.register(
        jf,
        "entries",
        "()Ljava/util/Enumeration;",
        p59_jar_file_entries,
    );

    // JarEntry extends ZipEntry. Two layouts coexist:
    //   * the 5-field SYNTHETIC stub (name=0, size=1, compressedSize=2,
    //     method=3, comment=4) produced by the native JarFile path, and
    //   * the REAL-JDK layout (14 inherited ZipEntry fields + 3 JarEntry
    //     fields) produced by `new JarEntry(...)` running real bytecode.
    // These natives shadow the real JarEntry methods, so they MUST handle both
    // layouts. A real JarEntry has many more than 4 instance fields; on that
    // layout the synthetic slot indices (1/2/3) point at `xdostime`/`mtime`/
    // `atime`, NOT `size`/`compressedSize`/`method`, so raw-slot access reads
    // and writes the wrong fields. Concretely, the old slot-3 `<init>` left the
    // real `method` field (slot 9) at 0 = STORED, so a default JarOutputStream
    // entry threw `ZipException: attempt to write past end of STORED entry`
    // (Mockito's inline-mock-maker bootstrap JAR, JaCoCo, etc.). Detect the
    // real layout by field count and use the by-name accessors there.
    let je = "java/util/jar/JarEntry";
    fn je_is_real_layout(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
    ) -> Result<bool, MethodCallFailed> {
        Ok(ctx.object_num_fields(this) > 5)
    }
    r.register(je, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if je_is_real_layout(ctx, this)? {
            // Mirror real ZipEntry.<init>(String): set `name` and the field
            // initializer defaults (xdostime/crc/size/csize/method = -1).
            ctx.set_field_by_name(this, "name", args[1]);
            ctx.set_field_by_name(this, "xdostime", Value::Long(-1));
            ctx.set_field_by_name(this, "crc", Value::Long(-1));
            ctx.set_field_by_name(this, "size", Value::Long(-1));
            ctx.set_field_by_name(this, "csize", Value::Long(-1));
            ctx.set_field_by_name(this, "method", Value::Int(-1));
        } else {
            ctx.set_field(this, 0, args[1]);
            ctx.set_field(this, 1, Value::Long(-1));
            ctx.set_field(this, 2, Value::Long(-1));
            ctx.set_field(this, 3, Value::Int(-1));
        }
        Ok(None)
    });
    r.register(je, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `name` is slot 0 in both layouts, but read by name on the real layout
        // for symmetry with the other accessors.
        if je_is_real_layout(ctx, this)? {
            Ok(Some(ctx.get_field_by_name(this, "name")))
        } else {
            Ok(Some(ctx.get_field(this, 0)))
        }
    });
    // JarEntry inherits ZipEntry accessors. Spring Boot's JarFileArchive
    // walks each entry via getName / isDirectory (for the include-filter)
    // and getComment (only consulted when checking the UNPACK: marker on
    // nested-JAR entries; we do not pack any UNPACK markers so null is
    // the right answer). Register them on JarEntry directly so the
    // override-allow-list (which forces native-only dispatch on certain
    // jar/zip entry points) sees a non-null result.
    r.register(je, "isDirectory", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name_val = if je_is_real_layout(ctx, this)? {
            ctx.get_field_by_name(this, "name")
        } else {
            ctx.get_field(this, 0)
        };
        let is_dir = match name_val {
            Value::Object(Some(s)) => {
                let name = ctx.read_string(s).unwrap_or_default();
                name.ends_with('/')
            }
            _ => false,
        };
        Ok(Some(Value::Int(if is_dir { 1 } else { 0 })))
    });
    r.register(je, "getComment", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if je_is_real_layout(ctx, this)? {
            Ok(Some(ctx.get_field_by_name(this, "comment")))
        } else {
            Ok(Some(ctx.get_field(this, 4)))
        }
    });
    // Was an unconditional null, so a jar's per-entry comment was invisible
    // even when the entry object genuinely carried one (Spring Boot's
    // `UNPACK:` marker is exactly such a comment). Real-layout entries have a
    // `comment` field — read it; the synthetic 4-slot layout does not track
    // comments, so null remains the honest answer there.
    r.register(je, "getComment", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if je_is_real_layout(ctx, this)? {
            return Ok(Some(ctx.get_field_by_name(this, "comment")));
        }
        Ok(Some(ctx.get_field(this, 4)))
    });
    r.register(je, "getSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = if je_is_real_layout(ctx, this)? {
            ctx.get_field_by_name(this, "size")
        } else {
            ctx.get_field(this, 1)
        };
        Ok(Some(match v {
            Value::Long(v) => Value::Long(v),
            Value::Int(v) => Value::Long(v as i64),
            _ => Value::Long(-1),
        }))
    });
    r.register(je, "getCompressedSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = if je_is_real_layout(ctx, this)? {
            ctx.get_field_by_name(this, "csize")
        } else {
            ctx.get_field(this, 2)
        };
        Ok(Some(match v {
            Value::Long(v) => Value::Long(v),
            Value::Int(v) => Value::Long(v as i64),
            _ => Value::Long(-1),
        }))
    });
    r.register(je, "getMethod", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = if je_is_real_layout(ctx, this)? {
            ctx.get_field_by_name(this, "method")
        } else {
            ctx.get_field(this, 3)
        };
        Ok(Some(match v {
            Value::Int(v) => Value::Int(v),
            _ => Value::Int(-1),
        }))
    });

    // Manifest = 2-field (mainAttrs=0, entries=1)
    let mf = "java/util/jar/Manifest";
    r.register(mf, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Empty main attributes + empty entries map so getMainAttributes /
        // getEntries always return a non-null container. Both helpers now run
        // real <init>s (which allocate and can GC-move objects), so pin `this`
        // and the attributes across the construction and re-read before storing.
        let this_pin = ctx.pin_native_root(this);
        let attrs = p59_manifest_new_attributes(ctx)?;
        let attrs_pin = ctx.pin_native_root(attrs);
        let entries = p59_manifest_new_entries_map(ctx)?;
        let this = ctx.read_native_pin(this_pin, this);
        let attrs = ctx.read_native_pin(attrs_pin, attrs);
        ctx.set_field(this, 0, Value::Object(Some(attrs)));
        ctx.set_field(this, 1, Value::Object(Some(entries)));
        ctx.set_field_by_name(this, "attr", Value::Object(Some(attrs)));
        ctx.set_field_by_name(this, "entries", Value::Object(Some(entries)));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });
    r.register(
        mf,
        "<init>",
        "(Ljava/io/InputStream;)V",
        p59_manifest_init_from_input_stream,
    );
    r.register(
        mf,
        "<init>",
        "(Ljava/io/InputStream;Ljava/lang/String;)V",
        p59_manifest_init_from_input_stream,
    );
    r.register(
        mf,
        "<init>",
        "(Ljava/util/jar/JarVerifier;Ljava/io/InputStream;Ljava/lang/String;)V",
        p59_manifest_init_from_verified_input_stream,
    );
    r.register(
        mf,
        "<init>",
        "(Ljava/util/jar/Manifest;)V",
        p59_manifest_init_copy,
    );
    r.register(
        mf,
        "getMainAttributes",
        "()Ljava/util/jar/Attributes;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let by_name = ctx.get_field_by_name(this, "attr");
            if matches!(by_name, Value::Object(Some(_))) {
                return Ok(Some(by_name));
            }
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(mf, "getEntries", "()Ljava/util/Map;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let by_name = ctx.get_field_by_name(this, "entries");
        if matches!(by_name, Value::Object(Some(_))) {
            return Ok(Some(by_name));
        }
        Ok(Some(ctx.get_field(this, 1)))
    });

    // NOTE: java.util.jar.Attributes is backed by a single real `map`
    // (LinkedHashMap) field — see p59_manifest_new_attributes. getValue /
    // putValue / size / entrySet therefore run the genuine JDK bytecode over
    // that map; we deliberately do NOT register synthetic overrides here (the
    // old slot-walking overrides were wrong for the 1-field real layout and
    // returned null/0 once the gen_heap OOB guard started dropping their
    // out-of-bounds slot accesses).

    // Attributes.Name constants
    let an = "java/util/jar/Attributes$Name";
    r.register(
        an,
        "MANIFEST_VERSION",
        "Ljava/util/jar/Attributes$Name;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/util/jar/Attributes$Name", 1)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh Name (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let s = ctx.create_string("Manifest-Version");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, 0, Value::Object(Some(s)));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        an,
        "MAIN_CLASS",
        "Ljava/util/jar/Attributes$Name;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/util/jar/Attributes$Name", 1)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh Name (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let s = ctx.create_string("Main-Class");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, 0, Value::Object(Some(s)));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // Spring Boot 3.x fat-jar launcher: short-circuit
    // `JarFileArchive.getClassPathUrls(Predicate, Predicate)` so we don't
    // depend on a working `Stream.map / Stream.filter / Stream.collect`
    // pipeline (which our synthetic Stream lacks). Read the JarFile field
    // directly, walk its central directory via the Rust `zip` crate, and
    // build a HashSet<URL> populated with `jar:nested:.../!/<entry>`
    // URLs for `BOOT-INF/lib/*.jar` and `BOOT-INF/classes/`.
    //
    // The synthetic HashSet (3-field: backing-array, size, capacity) is the
    // same shape every other Set.of native produces, so the downstream
    // `Launcher.createClassLoader(Collection)` -> `collection.toArray()`
    // round-trip lands the URLs in `URL[]` for the LaunchedClassLoader.
    let jfa = "org/springframework/boot/loader/launch/JarFileArchive";
    r.register(
        jfa,
        "getClassPathUrls",
        "(Ljava/util/function/Predicate;Ljava/util/function/Predicate;)Ljava/util/Set;",
        p59_spring_boot_jar_archive_get_class_path_urls,
    );
    r.register(
        "org/springframework/boot/loader/launch/ExplodedArchive",
        "getClassPathUrls",
        "(Ljava/util/function/Predicate;Ljava/util/function/Predicate;)Ljava/util/Set;",
        p59_spring_boot_exploded_archive_get_class_path_urls,
    );
    // `Archive.create(File)` chooses the archive implementation from
    // `File.isDirectory()`.  The real-JDK file bytecode can lose that fact for
    // a code-source directory under CratonVM, which made exploded launcher
    // tests run the JarFileArchive path against `build/classes/java/main`.
    // Preserve the defining filesystem kind here, and retain the real
    // JarFileArchive constructor for ordinary files.
    r.register(
        "org/springframework/boot/loader/launch/Archive",
        "create",
        "(Ljava/io/File;)Lorg/springframework/boot/loader/launch/Archive;",
        |ctx, args| {
            let file = obj_arg(args, 0)?;
            let path = file_read_path(ctx, file);
            if std::path::Path::new(&path).is_dir() {
                let archive = try_alloc_concurrent_synthetic(
                    ctx,
                    "org/springframework/boot/loader/launch/ExplodedArchive",
                    3,
                )?;
                ctx.set_field(archive, 0, Value::Object(Some(file)));
                let root_uri_path = ctx.create_string(&path.replace('\\', "/"));
                ctx.set_field(archive, 1, Value::Object(Some(root_uri_path)));
                return Ok(Some(Value::Object(Some(archive))));
            }
            ctx.new_object_initialized(
                "org/springframework/boot/loader/launch/JarFileArchive",
                "(Ljava/io/File;)V",
                &[Value::Object(Some(file))],
            )
        },
    );
    r.register(
        "org/springframework/boot/loader/launch/Archive",
        "create",
        "(Ljava/lang/Class;)Lorg/springframework/boot/loader/launch/Archive;",
        |ctx, _args| {
            // ExecutableArchiveLauncher invokes this with Launcher.class. The
            // classpath source is authoritative even when ProtectionDomain /
            // Path.of URI conversion loses the directory kind.
            let path = ctx
                .find_class_source_path("org/springframework/boot/loader/launch/Launcher")
                .unwrap_or_default();
            let file = file_alloc(ctx, &path)?;
            if std::path::Path::new(&path).is_dir() {
                let archive = try_alloc_concurrent_synthetic(
                    ctx,
                    "org/springframework/boot/loader/launch/ExplodedArchive",
                    3,
                )?;
                ctx.set_field(archive, 0, Value::Object(Some(file)));
                let root_uri_path = ctx.create_string(&path.replace('\\', "/"));
                ctx.set_field(archive, 1, Value::Object(Some(root_uri_path)));
                Ok(Some(Value::Object(Some(archive))))
            } else {
                ctx.new_object_initialized(
                    "org/springframework/boot/loader/launch/JarFileArchive",
                    "(Ljava/io/File;)V",
                    &[Value::Object(Some(file))],
                )
            }
        },
    );

    // Spring Boot 3.2+ repackaged launcher: `ExecutableArchiveLauncher` overrides
    // `createClassLoader(Collection)` and can CCE in `toArray` / typed iteration
    // under CratonVM. Register the bypass on every concrete launcher leaf as
    // well as the abstract base (same `check_override` shadowing issue as SB2).
    let sb3_launch_classes: &[&str] = &[
        "org/springframework/boot/loader/launch/ExecutableArchiveLauncher",
        "org/springframework/boot/loader/launch/JarLauncher",
        "org/springframework/boot/loader/launch/WarLauncher",
    ];
    for cls in sb3_launch_classes {
        r.register(
            cls,
            "createClassLoader",
            "(Ljava/util/Collection;)Ljava/lang/ClassLoader;",
            sb3_executable_archive_launcher_create_class_loader_collection,
        );
    }

    // -------------------------------------------------------------------
    // Spring Boot 2.x fat-jar launcher overrides.
    //
    // SB2 uses an OLDER launcher at `org.springframework.boot.loader.*`
    // (no `.launch.` subpackage). The launcher instance's `archive` field
    // is left null in our impl because the SB2 `JarFileArchive` constructor
    // chain depends on internal `org/springframework/boot/loader/jar/JarFile`
    // plumbing whose central-directory parser doesn't fully execute under
    // our synthetic-mode VM.  Rather than fix every transitive Stream /
    // RandomAccessDataFile / CentralDirectoryParser detail, we short-circuit
    // the three `ExecutableArchiveLauncher` methods that would otherwise
    // dereference the null archive — `getMainClass`, `isExploded`,
    // `getClassPathArchives` (SB2 v1) / `getClassPathArchivesIterator`
    // (SB2 v2.3+). Each native reads the fat-jar directly via the source
    // path resolved from the launcher's class mirror.
    // Register on every concrete SB2 launcher class plus the abstract
    // base. The parent-walk in `try_stackless_invoke` short-circuits when
    // the immediate parent has its own bytecode for the method, so a
    // native registered only on the abstract base would be shadowed by
    // EAL's own bytecode for a `JarLauncher` / `WarLauncher` receiver.
    // The duplicate registrations cost nothing and ensure the native
    // wins regardless of which leaf class the user invokes.
    let eal_classes: &[&str] = &[
        "org/springframework/boot/loader/ExecutableArchiveLauncher",
        "org/springframework/boot/loader/JarLauncher",
        "org/springframework/boot/loader/WarLauncher",
        "org/springframework/boot/loader/PropertiesLauncher",
    ];
    let launcher_base = "org/springframework/boot/loader/Launcher";
    // createArchive() on the abstract base — allocates a dummy
    // JarFileArchive-shaped object without running its constructor (and thus
    // without triggering JarFileArchive.<clinit> which accesses
    // PosixFilePermission.OWNER_READ — unavailable on Windows).
    // Our getClassPathArchivesIterator native re-derives the fat-jar path
    // from find_class_source_path so it never reads `this.archive`.
    r.register(
        launcher_base,
        "createArchive",
        "()Lorg/springframework/boot/loader/archive/Archive;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Build a minimal 2-field synthetic archive so downstream
            // getClassPathIndex / getUrl / isNestedArchive calls that are NOT
            // already overridden get a non-null receiver instead of NPE-ing.
            // Pin across the archive alloc below — a moving young GC there
            // would relocate `this` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let archive = try_alloc_concurrent_synthetic(
                ctx,
                "org/springframework/boot/loader/archive/JarFileArchive",
                4,
            )?;
            let this = ctx.read_native_pin(this_pin, this);
            // Write back into the launcher's `archive` field so bytecode
            // that does GETFIELD archive still gets a non-null value.
            ctx.set_field_by_name(this, "archive", Value::Object(Some(archive)));
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(archive))))
        },
    );

    r.register(
        launcher_base,
        "createClassLoader",
        "(Ljava/util/Iterator;)Ljava/lang/ClassLoader;",
        sb2_launcher_create_class_loader_bypass_archive_walk,
    );
    r.register(
        launcher_base,
        "createClassLoader",
        "(Ljava/util/List;)Ljava/lang/ClassLoader;",
        sb2_launcher_create_class_loader_bypass_archive_walk,
    );

    for cls in eal_classes {
        // Also register createArchive on each concrete launcher class so the
        // check_override path (which walks the class itself first) finds it.
        r.register(
            cls,
            "createArchive",
            "()Lorg/springframework/boot/loader/archive/Archive;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                // Pin across the archive alloc below — a moving young GC there
                // would relocate `this` (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let archive = try_alloc_concurrent_synthetic(
                    ctx,
                    "org/springframework/boot/loader/archive/JarFileArchive",
                    4,
                )?;
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field_by_name(this, "archive", Value::Object(Some(archive)));
                ctx.unpin_native_roots(this_pin);
                Ok(Some(Value::Object(Some(archive))))
            },
        );
        r.register(
            cls,
            "getMainClass",
            "()Ljava/lang/String;",
            sb2_launcher_get_main_class,
        );
        r.register(cls, "isExploded", "()Z", sb2_launcher_is_exploded);
        // KEEP: the real method is a `return <boolean literal>;` hook — a
        // per-class constant in Spring Boot itself, deprecated since 2.4 —
        // and its value is unobservable here regardless of which literal a
        // given launcher declares. Its ONLY reader is
        // `ExecutableArchiveLauncher.getClassPathArchivesIterator()` /
        // `getClassPathArchives()`, and both are natively replaced on these
        // same four classes (just below) *and* pinned in `vm_exec.rs`'s
        // force-native list, so Spring's post-processing branch is never
        // reached in either run mode. The previous comment's claim that
        // `ExecutableArchiveLauncher` itself returns false from 2.4 onwards
        // could not be confirmed against a Spring Boot source tree here; do
        // not treat it as established.
        r.register(
            cls,
            "isPostProcessingClassPathArchives",
            "()Z",
            |_ctx, _args| Ok(Some(Value::Int(0))),
        );
        r.register(
            cls,
            "getClassPathArchives",
            "()Ljava/util/List;",
            sb2_launcher_get_class_path_archives_list,
        );
        r.register(
            cls,
            "getClassPathArchivesIterator",
            "()Ljava/util/Iterator;",
            sb2_launcher_get_class_path_archives_iterator,
        );
        r.register(
            cls,
            "createClassLoader",
            "(Ljava/util/Iterator;)Ljava/lang/ClassLoader;",
            sb2_launcher_create_class_loader_bypass_archive_walk,
        );
        r.register(
            cls,
            "createClassLoader",
            "(Ljava/util/List;)Ljava/lang/ClassLoader;",
            sb2_launcher_create_class_loader_bypass_archive_walk,
        );
        // KEEP: `null` is what Spring Boot itself returns for the archive we
        // hand it. `ExecutableArchiveLauncher.getClassPathIndex(Archive)` is
        // a literal `return null;`, and the only override — `JarLauncher`'s —
        // loads `classpath.idx` *only* when `archive instanceof
        // ExplodedArchive`, falling through to `super` otherwise. Our
        // `createArchive` native always produces a `JarFileArchive`, so the
        // real code path being shadowed also returns null here. (Its sole
        // consumer, `getClassPathArchivesIterator`, is natively replaced
        // anyway.)
        r.register(
            cls,
            "getClassPathIndex",
            "(Lorg/springframework/boot/loader/archive/Archive;)Lorg/springframework/boot/loader/ClassPathIndexFile;",
            |_ctx, _args| Ok(Some(Value::Object(None))),
        );
    }

    // -------------------------------------------------------------------
    // Spring Boot 2.x: short-circuit `Handler.setUseFastConnectionExceptions`
    // to a no-op. The real method body delegates to
    // `JarURLConnection.setUseFastExceptions(boolean)`, which triggers
    // SB2's `JarURLConnection.<clinit>`. The static initialiser builds a
    // sentinel "not found" connection by calling the 3-arg private ctor
    // with `(null, null, null)`, which chains into
    // `java.net.JarURLConnection.<init>(URL)` ->
    // `java.net.URLConnection.<init>(URL)` -> `URL.getProtocol()` ->
    // `URLConnection.getDefaultUseCaches(protocol)` ->
    // `URL.lowerCaseProtocol(null).equals("jrt")` -> NPE
    // ("Cannot invoke equals on null"). Because the caller is
    // `LaunchedURLClassLoader.findResource` (called from
    // `ClassPathResource.exists` for the optional Spring `banner.gif/png/
    // jpg` lookup) the failure aborts `SpringApplication.run` before the
    // banner prints. The fast-exception flag is a pure perf hint; making
    // the setter a no-op preserves correctness while sidestepping the
    // cascading static init failure.
    //
    // KEEP: the real body's ONLY effect is the identity of the exception the
    // next failed nested-jar lookup throws — `JarURLConnection.notFound()`
    // hands back a shared pre-allocated, stack-trace-less
    // FileNotFoundException when the flag is set and a freshly filled-in one
    // when it is not. Both are caught and discarded by the same
    // `findResource` callers, so a no-op setter is behaviourally equivalent;
    // it is also the only version of this method that does not detonate
    // SB2's `JarURLConnection.<clinit>` (see above).
    let sb2_handler = "org/springframework/boot/loader/jar/Handler";
    r.register(
        sb2_handler,
        "setUseFastConnectionExceptions",
        "(Z)V",
        |_ctx, _args| Ok(None),
    );

    // SB2 LaunchedURLClassLoader.findResource(String): the real impl wraps a
    // call to super.findResource(name) (real-JDK URLClassLoader) in a
    // `Handler.setUseFastConnectionExceptions(true) ... (false)` bracket.
    // With the Handler stub above the bracket is harmless, but the
    // super.findResource path still walks each URL[] entry through
    // URLClassPath/URLUtil/URL.getDefaultPort, where a synthetic SB2 nested
    // jar URL (whose handler field was never populated by URL.<init>)
    // dereferences a null URLStreamHandler.
    //
    // The old body answered null UNCONDITIONALLY on the grounds that Spring
    // only reaches here for optional banner.gif/png/jpg lookups. That is not
    // true — `ClassPathResource.exists()` and every `ResourceLoader` probe
    // funnel through `findResource` as well, and an always-null result made
    // each of them report "absent" for resources demonstrably on the
    // classpath. `ucl_find_resource` IS the local `URLClassLoader.findResource`
    // this method delegates to (it resolves against the loader's own recorded
    // URLs, and is already what `URLClassLoader.findResource` is bound to), so
    // route there instead of skipping the lookup — which still avoids the SB2
    // nested-jar `Handler` walk described above.
    let luc = "org/springframework/boot/loader/LaunchedURLClassLoader";
    r.register(
        luc,
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        crate::classloader::ucl_find_resource,
    );

    // SB2 LaunchedURLClassLoader.loadClass(String, boolean) — the real
    // URLClassLoader bytecode for findClass walks URL entries via
    // JarURLConnection using the Spring Boot custom jar: Handler. The Handler
    // opens double-nested JAR URLs (jar:file:/fat.jar!/BOOT-INF/lib/dep.jar!/)
    // which we don't support in CratonVM's real-JDK mode. Instead, delegate
    // directly to CratonVM's built-in classpath scanner (ensure_class_initialized),
    // which already extracted all nested JARs from BOOT-INF/lib at startup.
    //
    // This native is registered on BOTH the SB2 and SB3 launcher class names
    // and for both the one-arg and two-arg overloads.
    r.register(
        luc,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("LaunchedURLClassLoader.loadClass: null name".to_string()),
                    }
                    .into())
                }
            };
            let dotted = ctx.read_string(name_obj).unwrap_or_default();
            let internal = dotted.replace('.', "/");
            match ctx.ensure_class_initialized(&internal) {
                Ok(class_id) => Ok(Some(Value::Object(Some(ctx.get_class_mirror(class_id))))),
                Err(_) => Err(RuntimeError::ClassNotFoundException { class_name: dotted }.into()),
            }
        },
    );
    r.register(
        luc,
        "loadClass",
        "(Ljava/lang/String;Z)Ljava/lang/Class;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("LaunchedURLClassLoader.loadClass(Z): null name".to_string()),
                    }
                    .into())
                }
            };
            let dotted = ctx.read_string(name_obj).unwrap_or_default();
            let internal = dotted.replace('.', "/");
            match ctx.ensure_class_initialized(&internal) {
                Ok(class_id) => Ok(Some(Value::Object(Some(ctx.get_class_mirror(class_id))))),
                Err(_) => Err(RuntimeError::ClassNotFoundException { class_name: dotted }.into()),
            }
        },
    );
    // ---------------------------------------------------------------------------
    // S111r21 — Spring's ClassUtils.forName(String, ClassLoader) native override.
    //
    // ClassUtils.forName is the entry point for all Spring factory class loading
    // (SpringFactoriesLoader.instantiateFactory calls it for every factory class).
    // The factory names in spring.factories use dotted canonical notation:
    //   "org.springframework.boot.web.servlet.context
    //      .AnnotationConfigServletWebServerApplicationContext.Factory"
    // (the inner class separator is `.`, not `$`).
    //
    // ClassUtils.forName calls Class.forName(name, false, classLoader) which
    // dispatches to our native_class_for_name. That in turn calls
    // invoke_virtual(classLoader, "loadClass", ...) which succeeds for the
    // outer class attempt but fails with InternalError(ClassNotFoundException)
    // for the ".Factory" suffix (because bytecode uses "$Factory"). This
    // InternalError is NOT caught by ClassUtils.forName's catch(ClassNotFoundException)
    // handler — it propagates up, bypassing the inner-class retry logic.
    //
    // By registering a native for ClassUtils.forName directly, we:
    //  1. Handle primitives and array types (Java primitive names, [] notation)
    //  2. Try direct binary name via ensure_class_initialized
    //  3. Try inner-class substitution (replace last `.` with `$`) if step 2 fails
    //  4. Throw ClassNotFoundException if both fail
    //
    // This is safe for all callers: ensure_class_initialized is the same loader
    // CratonVM uses for everything in BOOT-INF/lib/. The classLoader arg is
    // deliberately ignored (we use CratonVM's unified classpath scanner).
    // ---------------------------------------------------------------------------
    let cu = "org/springframework/util/ClassUtils";
    r.register(
        cu,
        "forName",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/Class;",
        spring_class_utils_for_name_impl,
    );
    // Some Spring Boot 3 / Spring Framework 6 code paths use a 1-arg overload.
    r.register(
        cu,
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        spring_class_utils_for_name_impl,
    );

    // S111r27 — intercept DefaultApplicationContextFactory.create to diagnose
    // what exception is thrown and provide a direct bypass if needed.
    //
    // DISABLED: the shim allocated a context with NO primary configuration source
    // registered, so SpringApplication.prepareContext -> load(...) never received
    // EurekaServerApplication.class as a @Configuration bean def. In real Spring
    // Boot, DefaultApplicationContextFactory.create just instantiates the context
    // and the caller does the load. Running the real bytecode is correct now that
    // HashMap / LinkedHashMap / MergedAnnotation / ClassLoader prerequisites are
    // fixed.
    let _ = spring_default_app_ctx_factory_create;
    // let dacf = "org/springframework/boot/DefaultApplicationContextFactory";
    // r.register(dacf, "create",
    //     "(Lorg/springframework/boot/WebApplicationType;)Lorg/springframework/context/ConfigurableApplicationContext;",
    //     spring_default_app_ctx_factory_create);
    r.set_category(__prev_cat);
}

/// S111r21 — native implementation of Spring's ClassUtils.forName(String, ClassLoader).
///
/// ClassUtils.forName is the entry point for all Spring factory class loading.
/// Factory names in spring.factories use dotted canonical notation:
///   "org.springframework.boot.web.servlet.context
///      .AnnotationConfigServletWebServerApplicationContext.Factory"
/// (where `.Factory` is the inner class — bytecode name uses `$Factory`).
///
/// The standard Java path through Class.forName → LaunchedURLClassLoader.loadClass
/// returns ClassNotFoundException as an InternalError from CratonVM's native layer;
/// this internal error bypasses ClassUtils.forName's `catch(ClassNotFoundException)`
/// handler, so the inner-class retry never runs.
///
/// By overriding ClassUtils.forName with this native we:
///  1. Try the direct binary name via ensure_class_initialized (fast path)
///  2. Try inner-class substitution (replace last '.' with '$') if step 1 fails
///  3. Throw ClassNotFoundException if both fail (propagated as Java-level CNFE)
pub(crate) fn spring_class_utils_for_name_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name_obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ClassUtils.forName: null name".to_string()),
            }
            .into())
        }
    };
    let dotted = ctx.read_string(name_obj).unwrap_or_default();

    // BUG-06 — this native shadows Spring's ClassUtils.forName, the engine
    // behind ClassUtils.isPresent. It must observe the same reflective-probe
    // semantics as the Class.forName native: an enterprise-framework class
    // (org/jboss/, io/smallrye/, …) that is not on the classpath must report
    // ABSENT (CNFE) rather than resolve to a fabricated synthetic stub.
    // Otherwise Spring's ReactiveAdapterRegistry sees a false-positive
    // `io.smallrye.mutiny.Multi`, registers MutinyRegistrar, and its <clinit>
    // dies on the incomplete stub. Real factory classes are on the classpath,
    // so the gate (which only fires after classpath lookup fails) leaves them
    // untouched. The guard clears on every return path below.
    let _probe_guard = cratonvm_types::reflective_probe::ProbeGuard::new();

    // Primitive language names resolve to the PRIMITIVE class (e.g. "int" ->
    // int.class), NOT the wrapper. Real Spring delegates to
    // resolvePrimitiveClassName, whose map is keyed by `primitiveType.getName()`
    // and holds the primitive `Class` objects. Returning the wrapper (Integer)
    // broke `<array value-type="int">` bean resolution —
    // `Array.newInstance(Integer.class, n)` yields `Integer[]`, not `int[]`, so
    // `(int[]) list.get(i)` threw `Integer cannot be cast to [I`
    // (CollectionsWithDefaultTypesTests.buildCollectionFromMixtureOfReferencesAndValues).
    match dotted.as_str() {
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void" => {
            let mirror = ctx.primitive_class_mirror(&dotted);
            return Ok(Some(Value::Object(Some(mirror))));
        }
        _ => {}
    }

    // Handle array types in Spring's source notation: "String[]", "int[]",
    // "Foo[][]", etc. Real Spring's `ClassUtils.forName` strips one "[]" and
    // recurses, then calls `Class.arrayType()`; we build the equivalent JVM
    // array descriptor directly. The component may be a primitive
    // ("int" → "I"), a reference type ("java.lang.String" → "Ljava/lang/String;"),
    // and the array may be multi-dimensional. The previous code unconditionally
    // built "[L<element>;" (single-dimension reference only), which produced a
    // bogus descriptor for primitive components ("[Lboolean;") and for
    // multi-dimensional arrays ("[Lorg/.../Foo[];") — making every such forName
    // fall through to ClassNotFoundException (ClassUtilsTests.forName /
    // forNameWithPrimitiveArrays, where "boolean[]", "int[]" and "Foo[][]" all
    // failed while single-dimension "java.lang.String[]" / "Foo[]" worked).
    if dotted.ends_with("[]") {
        // Peel trailing "[]" pairs → dimensionality + base component name.
        let mut base = dotted.as_str();
        let mut dims = 0usize;
        while let Some(stripped) = base.strip_suffix("[]") {
            base = stripped;
            dims += 1;
        }
        // Component descriptor: primitive letter, or `L<internal>;` reference
        // form. `void` has no array type (matches real `void.class.arrayType()`
        // throwing) → leave as None so we fall through to the CNFE path.
        let component = match base {
            "boolean" => Some("Z".to_string()),
            "byte" => Some("B".to_string()),
            "char" => Some("C".to_string()),
            "short" => Some("S".to_string()),
            "int" => Some("I".to_string()),
            "long" => Some("J".to_string()),
            "float" => Some("F".to_string()),
            "double" => Some("D".to_string()),
            "void" => None,
            _ => Some(format!("L{};", base.replace('.', "/"))),
        };
        if let Some(component) = component {
            let array_desc = format!("{}{component}", "[".repeat(dims));
            if let Ok(cid) = ctx.ensure_class_initialized(&array_desc) {
                return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
            }
        }
    }

    // Classloader-isolation: when an explicit, *user-defined* ClassLoader is
    // supplied (e.g. Spring's OverridingClassLoader / a FilteringClassLoader),
    // resolve the class THROUGH that loader instead of CratonVM's global
    // classpath scanner. Real Spring's `ClassUtils.forName` calls
    // `Class.forName(name, false, classLoader)`, whose defining-loader identity
    // and override-first ordering are essential to classloader-isolation
    // patterns: a child loader must define its OWN copy of an eligible class
    // (even when the parent already loaded one), and Class-valued annotation
    // members must then resolve through that child loader. Ignoring the loader
    // here made every such class resolve to the app-loaded copy, defeating the
    // isolation — so a FilteringClassLoader that rejects `*Filtered*` types was
    // bypassed (no deferred `TypeNotPresentException`) and synthesized
    // annotations reported the app loader instead of the child
    // (AnnotationIntrospectionFailureTests, MergedAnnotationClassLoaderTests,
    // TypeMappedAnnotationTests.adaptFromStringToClassWithMemberSourceUsesMemberClassLoader).
    //
    // Built-in loaders (app/platform/boot) and a null loader keep the global
    // scanner below — that path resolves CratonVM's unified classpath (incl.
    // BOOT-INF/lib fat-jars) and carries the LaunchedURLClassLoader rescue that
    // routing through `loadClass` would lose; its inner-class retry also covers
    // the Spring-Boot `a.b.Outer.Factory` → `a/b/Outer$Factory` factory names.
    // Spring's ClassUtils substitutes getDefaultClassLoader() when the caller
    // passes null. That is normally the thread context class loader (TCCL),
    // which is how ModifiedClassPathExtension makes ClassUtils.isPresent
    // observe its filtered class path. The previous native treated null as the
    // unified application class path, leaking excluded clients back into
    // ClientHttpRequestFactoryBuilder.detect().
    //
    // Use the Thread accessor rather than reading its field directly: the
    // accessor owns the real-JDK layout and explicit-null handling. As with
    // Spring's getDefaultClassLoader(), an unavailable/null TCCL falls through
    // to the normal application-loader path below.
    let current_thread = ctx.current_thread_object();
    let loader = match args.get(1) {
        Some(Value::Object(Some(loader))) => Some(*loader),
        _ => match ctx.invoke_virtual(
            current_thread,
            "getContextClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(loader)))) => Some(loader),
            _ => None,
        },
    };
    // Fork-loader identity fix (2026-07-22 AOT bean-override session): when
    // the resolved `loader` above is None (no explicit arg, no TCCL) or is a
    // built-in loader that `is_user_defined_loader` rejects, this native
    // previously fell straight through to the global, loader-BLIND
    // `ensure_class_initialized` fast path below. That path prefers an
    // already-loaded Application-loader answer over a class the CALLER's own
    // `@CompileWithForkedClassLoader` fork loader already redefined (see
    // `classloading::class_manager::resolve_fast_path_class_id`'s own doc
    // comment), so framework infrastructure classes resolved via
    // `ClassUtils.forName(name, null)` from code running INSIDE a fork (e.g.
    // `GeneratedMapUtils.loadMap` -> `AotTestContextInitializersFactory`)
    // silently collapsed onto a FRESH Application-loader `ClassId` distinct
    // from the fork's own copy -- busting identity-based caches keyed off the
    // resulting `Class` object (`AotMergedContextConfiguration.hashCode()`),
    // and ultimately causing a SECOND, uncustomized `ApplicationContext` to
    // be created and used in place of the properly `@TestBean`/`@MockitoBean`
    // -overridden one (see CRATONVM-SPRING-GENUINE-BUGLIST,
    // AOT cluster, 2026-07-22 bean-override session). Mirror `Class.forName`'s
    // OWN caller-sensitive fallback here too: if the immediate Java caller of
    // this native (i.e. of `ClassUtils.forName` itself) was defined by a
    // user-defined loader, prefer resolving through THAT loader before ever
    // reaching the global fallback -- exactly the same rule already applied
    // above when `loader` came from TCCL/an explicit arg.
    let loader = loader.or_else(|| crate::lang_class::class_for_name_one_arg_caller_loader(ctx));

    if let Some(loader) = loader {
        if crate::classloader::is_user_defined_loader(ctx, loader) {
            let load = |ctx: &mut dyn NativeContext, n: ObjectRef| {
                crate::lang_class::native_class_for_name(
                    ctx,
                    &[
                        Value::Object(Some(n)),
                        Value::Int(0),
                        Value::Object(Some(loader)),
                    ],
                )
            };
            match load(ctx, name_obj) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // Mirror Spring's inner-class retry through the SAME loader:
                    // replace the last package separator with `$` and try once
                    // more before surfacing the original failure.
                    let internal = dotted.replace('.', "/");
                    if let Some(last_slash) = internal.rfind('/') {
                        let inner = format!(
                            "{}${}",
                            &internal[..last_slash],
                            &internal[last_slash + 1..]
                        );
                        let inner_dotted = inner.replace('/', ".");
                        let inner_obj = ctx.create_string(&inner_dotted);
                        if let Ok(v) = load(ctx, inner_obj) {
                            return Ok(v);
                        }
                    }
                    return Err(e);
                }
            }
        }
    }

    // Platform/bootstrap loader: the JLS guarantees these can never see
    // application classes, no matter how permissive the "built-in loader ->
    // global scanner" fallback above is for the (very different) app-loader
    // case, whose whole job IS to see the unified classpath. Without this,
    // `ClassUtils.isPresent(appClassName, ClassLoader.getPlatformClassLoader())`
    // false-positived every application class as present via the same
    // global scanner an app-loader lookup legitimately uses, breaking any
    // "is X absent from a restricted loader" check (e.g.
    // `LogbackRuntimeHints#registerHints` gating on whether logback is on
    // the given loader — see
    // fixed-suite-bugs/springboot/classutils-forname-platform-loader-false-positive.md).
    if let Some(loader) = loader {
        let is_platform_or_boot = matches!(
            ctx.class_name_arc_of_id(ctx.class_id_of_object(loader))
                .as_deref(),
            Some("jdk/internal/loader/ClassLoaders$PlatformClassLoader")
                | Some("jdk/internal/loader/ClassLoaders$BootClassLoader")
        );
        if is_platform_or_boot {
            let internal = dotted.replace('.', "/");
            if !crate::classloader::is_bootstrap_class_name(&internal) {
                return Err(RuntimeError::ClassNotFoundException { class_name: dotted }.into());
            }
            // Genuinely bootstrap-owned name (java.*, jdk.*, ...) — fall
            // through to the normal resolution below.
        }
    }

    // Regular class name: try direct binary name first (a.b.Foo → a/b/Foo)
    let internal = dotted.replace('.', "/");
    if let Ok(cid) = ctx.ensure_class_initialized(&internal) {
        return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
    }

    // Inner-class retry: replace last '/' with '$'
    // "a/b/Outer/Inner" → "a/b/Outer$Inner"
    if let Some(last_slash) = internal.rfind('/') {
        let inner = format!(
            "{}${}",
            &internal[..last_slash],
            &internal[last_slash + 1..]
        );
        if let Ok(cid) = ctx.ensure_class_initialized(&inner) {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
        }
    }

    if dotted == "jakarta.faces.context.FacesContext" {
        return Ok(Some(Value::Object(None)));
    }
    Err(RuntimeError::ClassNotFoundException { class_name: dotted }.into())
}

// S111r27: Interceptor for DefaultApplicationContextFactory.create.
// When factories can't be loaded correctly (e.g. HashMap layout mismatch or
// ConcurrentReferenceHashMap cache miss), this native provides a direct bypass:
// it creates AnnotationConfigServletWebServerApplicationContext directly for
// SERVLET type applications.
pub(crate) fn spring_default_app_ctx_factory_create(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[1] = WebApplicationType enum instance
    let web_type_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let is_servlet = match web_type_val {
        Value::Object(Some(wt)) => {
            // WebApplicationType enum - read the name from field 0 (the enum name)
            // or from the ordinal. The name "SERVLET" is what we're looking for.
            let name_from_field = match ctx.get_field(wt, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // Also check read_string on the enum itself
            let enum_str = ctx.read_string(wt).unwrap_or_default();
            name_from_field.contains("SERVLET") || enum_str.contains("SERVLET")
        }
        _ => false,
    };

    // Attempt to create the right context class
    let ctx_class = if is_servlet {
        "org/springframework/boot/web/servlet/context/AnnotationConfigServletWebServerApplicationContext"
    } else {
        // For NONE type fall back to AnnotationConfigApplicationContext
        "org/springframework/context/annotation/AnnotationConfigApplicationContext"
    };

    // Step 1: allocate the object (new_object does NOT run constructor)
    let obj_val = match ctx.new_object(ctx_class) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => {
            return Err(e);
        }
    };

    // Step 2: run the no-arg constructor explicitly via invoke_special
    // invoke_special args[0] = this
    match ctx.invoke_special(ctx_class, "<init>", "()V", &[obj_val]) {
        Ok(_) => Ok(Some(obj_val)),
        Err(e) => Err(e),
    }
}

// =============================================================================
// JarFile.stream() / .entries() — Spring Boot 3 fat-jar launcher support.
// Reads the central directory of the JAR backing the synthetic JarFile and
// builds a synthetic Stream<JarEntry> / Enumeration<JarEntry> populated with
// 4-field synthetic JarEntry instances (name, size, compressedSize, method).
// =============================================================================

/// One central-directory entry's metadata. Deliberately excludes decompressed
/// bytes — see `jar_contents_cached`'s doc comment for why those are cached
/// separately (and lazily) via `jar_entry_bytes_cached` instead of here.
pub(crate) struct JarEntryRec {
    pub(crate) size: i64,
    pub(crate) csize: i64,
    pub(crate) method: i32,
    pub(crate) crc: i64,
    pub(crate) comment: Option<String>,
    pub(crate) times: JarEntryTimes,
}

/// ZIP extended timestamp fields are authoritative when a JDK-created entry
/// has a DOS date of 1980. The latter is a lossy fallback, while the extra
/// fields retain the actual FileTime values.
#[derive(Clone, Copy, Default)]
pub(crate) struct JarEntryTimes {
    /// The central-directory DOS timestamp packed as `(date << 16) | time`,
    /// i.e. the low half of the real `ZipEntry.xdostime`.
    ///
    /// Entries written without any `FileTime` carry no 0x5455 extra field at
    /// all, so `modified` is `None` and `getLastModifiedTime()` falls through
    /// to `xdostime`. We allocate the `JarEntry` without running `<init>`, so
    /// that field starts at 0 rather than the JDK's `-1` sentinel, and 0
    /// decodes as `1979-11-30T00:00:16Z` instead of the real timestamp. Carry
    /// the DOS value so the fallback lands where HotSpot's does.
    pub(crate) dos_time: Option<i64>,
    pub(crate) modified: Option<i64>,
    pub(crate) access: Option<i64>,
    pub(crate) creation: Option<i64>,
}

/// Whole-jar parsed contents: per-name records plus central-directory order.
pub(crate) struct JarContents {
    pub(crate) by_name: std::collections::HashMap<String, JarEntryRec>,
    pub(crate) order: Vec<String>,
    /// Memoized `Multi-Release: true` answer (JEP 238) for this jar.
    ///
    /// `p59_jar_lookup_versioned_entry` asks once per ENTRY lookup, and
    /// answering costs a whole-manifest `from_utf8_lossy` plus a line-by-line
    /// parse. On a Tomcat webapp deploy - hundreds of jars, thousands of entry
    /// lookups - that re-parse was 8.3% of the entire run. Living on
    /// `JarContents` keys it on the same (path, mtime) pair as the rest of the
    /// jar cache, so it invalidates exactly when the jar does.
    pub(crate) multi_release: std::sync::OnceLock<bool>,
}

/// The three `ZipEntry` time attributes for Spring Boot's cached `JarEntryRec`.
///
/// `extra_data_fields()` yields the **central-directory** extras, which is the
/// same record HotSpot's `ZipFile`/`JarFile` read. For a JDK-written entry the
/// central 0x5455 payload holds the modified time alone (5 bytes) even though
/// its flags byte claims all three, so access and creation come back `None`
/// and `getLastAccessTime()`/`getCreationTime()` answer `null` — exactly as on
/// HotSpot. Do not supplement this from the local file header: that carries
/// all three, and merging it in broke parity while costing a `File::open` plus
/// two seeks and a read per entry. See `native-io/src/zip_real_jar.rs`'s
/// `ZipEntryTimes` for the byte-level detail.
pub(crate) fn p59_zip_entry_times(entry: &zip::read::ZipFile<'_>) -> JarEntryTimes {
    let mut times = JarEntryTimes {
        dos_time: entry
            .last_modified()
            .map(|time| (i64::from(time.datepart()) << 16) | i64::from(time.timepart())),
        ..JarEntryTimes::default()
    };
    for field in entry.extra_data_fields() {
        let parsed = match field {
            zip::extra_fields::ExtraField::ExtendedTimestamp(timestamp) => JarEntryTimes {
                modified: timestamp.mod_time().map(|time| i64::from(time) * 1_000),
                access: timestamp.ac_time().map(|time| i64::from(time) * 1_000),
                creation: timestamp.cr_time().map(|time| i64::from(time) * 1_000),
                ..JarEntryTimes::default()
            },
            zip::extra_fields::ExtraField::Ntfs(timestamp) => JarEntryTimes {
                modified: Some(p59_windows_filetime_to_unix_millis(timestamp.mtime())),
                access: Some(p59_windows_filetime_to_unix_millis(timestamp.atime())),
                creation: Some(p59_windows_filetime_to_unix_millis(timestamp.ctime())),
                ..JarEntryTimes::default()
            },
        };
        p59_merge_zip_times(&mut times, parsed);
    }
    times
}

pub(crate) fn p59_windows_filetime_to_unix_millis(time: u64) -> i64 {
    (i128::from(time) / 10_000 - 11_644_473_600_000i128) as i64
}

pub(crate) fn p59_merge_zip_times(target: &mut JarEntryTimes, source: JarEntryTimes) {
    if source.modified.is_some() {
        target.modified = source.modified;
    }
    if source.access.is_some() {
        target.access = source.access;
    }
    if source.creation.is_some() {
        target.creation = source.creation;
    }
}

pub(crate) fn p59_set_jar_entry_times(
    ctx: &mut dyn NativeContext,
    entry: ObjectRef,
    times: JarEntryTimes,
) {
    let entry_pin = ctx.pin_native_root(entry);
    // Before the FileTime fields, because this is the fallback they shadow:
    // `getLastModifiedTime()` only consults `xdostime` when `mtime` is null.
    if let Some(dos_time) = times.dos_time {
        ctx.set_field_by_name(entry, "xdostime", Value::Long(dos_time));
    }
    for (field, millis) in [
        ("mtime", times.modified),
        ("atime", times.access),
        ("ctime", times.creation),
    ] {
        let Some(millis) = millis else {
            continue;
        };
        let time = match ctx.invoke(
            "java/nio/file/attribute/FileTime",
            "fromMillis",
            "(J)Ljava/nio/file/attribute/FileTime;",
            &[Value::Long(millis)],
        ) {
            Ok(Some(Value::Object(Some(time)))) => time,
            _ => continue,
        };
        let time_pin = ctx.pin_native_root(time);
        let entry = ctx.read_native_pin(entry_pin, entry);
        let time = ctx.read_native_pin(time_pin, time);
        ctx.set_field_by_name(entry, field, Value::Object(Some(time)));
        ctx.unpin_native_roots(time_pin);
    }
    ctx.unpin_native_roots(entry_pin);
}

/// The last modification time this process has observed for `path`, in
/// nanoseconds since the epoch, or 0 if the file could not be stat'ed.
///
/// # Why this is memoised and not simply stat'ed
///
/// `jar_contents_cached` and `jar_entry_bytes_cached` key on `(path, mtime)`
/// so a jar rewritten on disk is not served stale. Deriving that key used to
/// mean a `std::fs::metadata` call **on every accessor call**, and on Windows
/// `std::fs::metadata` opens a file handle (`CreateFileW` +
/// `GetFileInformationByHandle` + `CloseHandle`), which measured **~45 us per
/// call** on the development host — three orders of magnitude more than the
/// cache lookup it was guarding.
///
/// That is not a rounding error on the workloads this file exists for.
/// Jasper's TLD scan drives Tomcat's `JarFileUrlJar.nextEntry()`, which on a
/// multi-release jar calls `JarFile.getJarEntry(name)` once per entry; each of
/// those reaches `jar_contents_cached` up to three times
/// (`p59_jar_is_multi_release`, the versioned-name search, and the entry
/// lookup itself). `probes/TldJarScanProbe.java` walks a 130-jar, 32 491-entry
/// classpath the way that scan does: **789-1720 ms on CratonVM against 40-58 ms
/// on HotSpot**, with the phase split putting 560-2474 ms of it in the 11 751
/// `getJarEntry` re-lookups alone. `TomcatServletWebServerFactoryTests` does
/// that walk once per embedded-container start, 121 times.
///
/// # What is given up, and why it is the right trade
///
/// The probe now happens once per path, plus once more whenever a `JarFile` /
/// `ZipFile` is *constructed* for it ([`jar_cache_revalidate`]). So a jar
/// rewritten on disk and then **reopened** still gets a fresh parse — which is
/// the case the `(path, mtime)` key was introduced for — while a rewrite seen
/// through an already-open handle keeps serving the snapshot taken at open.
///
/// That is HotSpot's behaviour, not a weakening of it: the JDK's
/// `ZipFile.Source` cache is keyed on `(file, lastModified, size)` sampled in
/// `ZipFile.Source.get()` at open time and never re-sampled, and the open file
/// is held for the life of the `ZipFile`. Re-stat-per-accessor was stricter
/// than the thing it was emulating.
fn jar_path_mtime(path: &str) -> u128 {
    // Copy the hit out under an explicit, closed scope rather than in an
    // `if let` condition. A guard held by an `if let` scrutinee outlives the
    // body, so the miss path below would still be holding this non-reentrant
    // mutex when `jar_path_mtime_probe` re-locks it — the exact deadlock
    // `jar_entry_bytes_cached` already documents having hit with `ARCHIVES`.
    // `parking_lot` gives no second chance there: it blocks, it does not
    // report a poisoned re-entry.
    let hit = { mtime_memo().lock().get(path).copied() };
    match hit {
        Some(m) => m,
        None => jar_path_mtime_probe(path),
    }
}

/// Path → last observed mtime. See [`jar_path_mtime`] for why it is memoised.
fn mtime_memo() -> &'static OrderedPlMutex<std::collections::HashMap<String, u128>> {
    // LEVEL (lock-discipline ratchet): `Scratch` is L0, the bottom of the
    // hierarchy — a thread holding it may acquire NOTHING else. This memo
    // meets that claim about as plainly as a lock in this crate can: it has
    // exactly two critical sections, a `get(&str).copied()` here and an
    // `insert(String, u128)` in `jar_path_mtime_probe`, both on a plain
    // `HashMap` of owned values. Neither function takes a `NativeContext`, so
    // neither *can* re-enter the VM, and neither takes another lock.
    //
    // Both callers that pair this with a jar cache — `jar_contents_cached` and
    // `jar_entry_bytes_cached` — read the mtime FIRST and lock their own cache
    // afterwards, so this is never the outer lock of a pair either. Keep it
    // that way: the mtime is only wanted to build a cache key, so there is no
    // reason to still hold it once the key exists.
    //
    // Why the bottom and not some other free level: this crate re-enters the
    // VM constantly (a native callback calls back into Java, taking the heap
    // and the L10 class-manager lock), so anything held across that re-entry
    // is a cycle. L0 says this one never is, and makes a future violation a
    // checker failure instead of a hang.
    //
    // `OnceLock` rather than a `static` initialiser like `AOT_CACHE_INPUT_PATH`
    // only because `HashMap::new` is not `const`.
    static MEMO: std::sync::OnceLock<OrderedPlMutex<std::collections::HashMap<String, u128>>> =
        std::sync::OnceLock::new();
    MEMO.get_or_init(|| OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch))
}

/// Stat `path` for real and record the answer.
///
/// A failed stat is deliberately NOT memoised: a path that does not exist yet
/// (a jar about to be written) must not be pinned to 0 for the life of the
/// process.
fn jar_path_mtime_probe(path: &str) -> u128 {
    let probed = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos());
    match probed {
        Some(mtime) => {
            mtime_memo().lock().insert(path.to_string(), mtime);
            mtime
        }
        None => 0,
    }
}

/// Re-stat `path`, so a jar rewritten since the last probe is picked up.
///
/// Call this from every place that OPENS an archive. Nothing else has to
/// happen: both jar caches key on the mtime, so a changed value simply
/// produces a different key and the next lookup parses afresh.
pub(crate) fn jar_cache_revalidate(path: &str) {
    if !path.is_empty() {
        jar_path_mtime_probe(path);
    }
}

/// Per-path cache of a JAR's parsed central directory (metadata only — no
/// decompressed bytes; see `jar_entry_bytes_cached` for those).
///
/// The `java.util.jar.JarFile` natives (`getInputStream`/`getEntry`/`entries`/
/// `stream`/lookup) previously called `zip::ZipArchive::new(file)` on EVERY
/// call, which re-reads and re-parses the whole central directory each time.
/// Tomcat's `ContextConfig` annotation scanner calls `getInputStream` once per
/// `.class` entry, making that O(N²) over a jar's entry count — for a large jar
/// like byte-buddy (~3k classes) the web-fragment scan never finishes within
/// the test timeout (TestValidator HANG; it passes on HotSpot where each lookup
/// is O(1)). Parse once and cache, keyed by (path, mtime) so a jar rewritten on
/// disk (e.g. a test-generated temp jar) is not served stale.
///
/// PERF (2026-07-23): this cache used to also eagerly `read_to_end` (i.e.
/// fully INFLATE) every entry's bytes on the very first touch, regardless of
/// whether the caller wanted bytes at all. `getJarEntry`/`entries`/`stream`/
/// `getManifest` only need metadata (size/csize/method/crc/times) — a single
/// `ClassUtils.isPresent()`-style existence check on a jar with thousands of
/// classes (e.g. testcontainers.jar, 12.5k entries) was paying the FULL
/// decompression cost of every unrelated entry (measured ~60-70us/entry —
/// genuine DEFLATE work, not native-dispatch overhead) just to answer one
/// membership question. On `module/spring-boot-data-redis`'s ~121-jar test
/// classpath this made ordinary Spring context bootstrap (which does hundreds
/// of such isPresent/loadClass checks) blow past the 300s suite timeout —
/// `DataRedisAutoConfigurationTests`, `DataRedisAutoConfigurationJedisTests`,
/// `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests`, and
/// `DataRedisHealthContributorAutoConfigurationTests` all HANG. Bytes are now
/// decompressed lazily, per-entry, only when `getInputStream` is actually
/// called for that entry — see `jar_entry_bytes_cached`.
pub(crate) fn jar_contents_cached(path: &str) -> Option<std::sync::Arc<JarContents>> {
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, Arc<JarContents>>>> =
        OnceLock::new();
    if path.is_empty() {
        return None;
    }
    let mtime = jar_path_mtime(path);
    let key = format!("{path}\u{0}{mtime}");
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(c) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(c.clone());
    }
    // Build outside the lock (decompression can be slow); a concurrent racer
    // just rebuilds and the last writer wins — the contents are identical.
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let len = archive.len();
    let mut by_name = std::collections::HashMap::with_capacity(len);
    let mut order = Vec::with_capacity(len);
    for i in 0..len {
        // Metadata only — deliberately no `read_to_end`/decompression here.
        // `by_index` parses the local file header (cheap: no inflate), which
        // is enough for every field below. See the doc comment above for why
        // eagerly decompressing was a severe perf bug.
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        let name = entry.name().to_string();
        let size = entry.size() as i64;
        let csize = entry.compressed_size() as i64;
        #[allow(deprecated)]
        let method = entry.compression().to_u16() as i32;
        let crc = entry.crc32() as i64 & 0xFFFF_FFFFi64;
        let comment = (!entry.comment().is_empty()).then(|| entry.comment().to_owned());
        let times = p59_zip_entry_times(&entry);
        order.push(name.clone());
        by_name.insert(
            name,
            JarEntryRec {
                size,
                csize,
                method,
                crc,
                comment,
                times,
            },
        );
    }
    let contents = Arc::new(JarContents {
        by_name,
        order,
        multi_release: std::sync::OnceLock::new(),
    });
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, contents.clone());
    Some(contents)
}

/// Per-(path, mtime, entry name) cache of ONE entry's decompressed bytes.
/// Companion to `jar_contents_cached`: that cache is metadata-only (cheap,
/// built eagerly for the whole jar); this one does the actual DEFLATE
/// inflate, lazily, only for entries some caller's `getInputStream` actually
/// reads. Re-opens the archive and seeks straight to the named entry rather
/// than iterating — `by_name` on a `zip::ZipArchive` uses its already-parsed
/// central-directory name index, so this stays cheap even on jars with
/// thousands of entries.
pub(crate) fn jar_entry_bytes_cached(
    path: &str,
    entry_name: &str,
) -> Option<std::sync::Arc<Vec<u8>>> {
    use std::io::Read;
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, Arc<Vec<u8>>>>> =
        OnceLock::new();
    // Keep the parsed central directory alive across distinct entries. The
    // signed-JAR parity test drains every entry, so reopening ZipArchive per
    // cache miss makes archive traversal quadratic in the entry count.
    static ARCHIVES: OnceLock<
        Mutex<std::collections::HashMap<String, Arc<Mutex<zip::ZipArchive<std::fs::File>>>>>,
    > = OnceLock::new();
    if path.is_empty() || entry_name.is_empty() {
        return None;
    }
    let mtime = jar_path_mtime(path);
    let key = format!("{path}\u{0}{mtime}\u{0}{entry_name}");
    let archive_key = format!("{path}\u{0}{mtime}");
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(b) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(b.clone());
    }
    let archives = ARCHIVES.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    // Take the cache hit under a short, explicit guard scope. An `if let`
    // condition keeps its temporary alive across the whole expression; on a
    // miss the old form then attempted to lock this same non-reentrant mutex
    // again in `else`, deadlocking the very first JarFile manifest read.
    let cached_archive = {
        let archives = archives.lock().unwrap_or_else(|e| e.into_inner());
        archives.get(&archive_key).cloned()
    };
    let archive = if let Some(archive) = cached_archive {
        archive.clone()
    } else {
        // Construct outside the registry lock: central-directory parsing can
        // be costly and a concurrent first reader can safely race.
        let file = std::fs::File::open(path).ok()?;
        let opened = Arc::new(Mutex::new(zip::ZipArchive::new(file).ok()?));
        let mut archives = archives.lock().unwrap_or_else(|e| e.into_inner());
        archives
            .entry(archive_key)
            .or_insert_with(|| opened.clone())
            .clone()
    };
    let mut archive = archive.lock().unwrap_or_else(|e| e.into_inner());
    let mut entry = archive.by_name(entry_name).ok()?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).ok()?;
    let bytes = Arc::new(buf);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, bytes.clone());
    Some(bytes)
}

/// Read the central directory of `path` and return a Vec of allocated
/// synthetic `java/util/jar/JarEntry` ObjectRefs. Returns an empty Vec on
/// any I/O / zip-parse error so callers see an empty Stream rather than
/// an exception path.
pub(crate) fn p59_jar_collect_entries(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<Vec<Value>, MethodCallFailed> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let contents = match jar_contents_cached(path) {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(contents.order.len());
    // Pin each fresh entry across the subsequent per-entry allocs — a moving
    // young GC there would relocate the earlier entries (native stale-local
    // family). The pins stay live past this helper (the VM truncates the pin
    // vec when the enclosing native returns); the refs are re-read to their
    // current addresses right before returning, so callers receive values
    // that are valid until their own next allocating call.
    let mut pins: Vec<usize> = Vec::with_capacity(contents.order.len());
    for name in &contents.order {
        let rec = match contents.by_name.get(name) {
            Some(r) => r,
            None => continue,
        };
        let (size, csize, method, crc, comment, times) = (
            rec.size,
            rec.csize,
            rec.method,
            rec.crc,
            rec.comment.clone(),
            rec.times,
        );
        let je = try_alloc_concurrent_synthetic(ctx, "java/util/jar/JarEntry", 5)?;
        let je_pin = ctx.pin_native_root(je);
        let comment_s = comment.map(|comment| ctx.create_string(&comment));
        let comment_pin = comment_s.map(|comment| ctx.pin_native_root(comment));
        let name_s = ctx.create_string(name);
        let je = ctx.read_native_pin(je_pin, je);
        let comment_s = comment_s.zip(comment_pin).map(|(comment, pin)| {
            let comment = ctx.read_native_pin(pin, comment);
            ctx.unpin_native_roots(pin);
            comment
        });
        pins.push(je_pin);
        ctx.set_field(je, 0, Value::Object(Some(name_s)));
        ctx.set_field(je, 1, Value::Long(size));
        ctx.set_field(je, 2, Value::Long(csize));
        ctx.set_field(je, 3, Value::Int(method));
        ctx.set_field(je, 4, Value::Object(comment_s));
        // Real-JDK mode: `ZipEntry.getSize()/getMethod()/getCompressedSize()/getCrc()`
        // read the REAL fields by their actual offset, not the synthetic slots
        // above (real order: name,xdostime,crc,size,csize,method,…). Tomcat's
        // webapp class loader sizes its class-byte read from `entry.getSize()`;
        // a 0 yields a 0-length class → ClassFormatError "class file too short"
        // (JSTL JstlCoreTLV). Mirror the real field names (same fix as
        // `p59_jar_lookup_entry`).
        ctx.set_field_by_name(je, "name", Value::Object(Some(name_s)));
        ctx.set_field_by_name(je, "size", Value::Long(size));
        ctx.set_field_by_name(je, "csize", Value::Long(csize));
        ctx.set_field_by_name(je, "method", Value::Int(method));
        ctx.set_field_by_name(je, "crc", Value::Long(crc));
        ctx.set_field_by_name(je, "comment", Value::Object(comment_s));
        p59_set_jar_entry_times(ctx, je, times);
        out.push(Value::Object(Some(je)));
    }
    // Re-read every entry to its current (post-GC) address before returning.
    for (v, h) in out.iter_mut().zip(&pins) {
        if let Value::Object(Some(o)) = v {
            *v = Value::Object(Some(ctx.read_native_pin(*h, *o)));
        }
    }
    Ok(out)
}

/// Resolve a JarFile lookup using the same multi-release entry selection as the
/// JDK. A selected versioned entry needs a `JarFileEntry`: its inherited
/// ZipEntry name stays physical while its `basename` is the caller's logical
/// name, which is precisely the `getRealName`/`getName` split that callers use.
fn p59_jar_lookup_versioned_entry(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    path: &str,
    entry_name: &str,
) -> MethodCallResult {
    // Read the feature version straight from the system properties instead of
    // materialising a `Runtime$Version` per entry lookup: `Runtime.version()`
    // now populates the object's four real fields (a `List<Integer>`, three
    // `Optional`s), which is several VM re-entries of work to then throw away.
    let runtime_feature = crate::lang_system::runtime_feature_version(ctx).max(8);
    let physical_name = if !entry_name.starts_with("META-INF/")
        && runtime_feature > 8
        && p59_jar_is_multi_release(path)
    {
        jar_contents_cached(path).and_then(|contents| {
            (9..=runtime_feature).rev().find_map(|version| {
                let candidate = format!("META-INF/versions/{version}/{entry_name}");
                contents
                    .by_name
                    .contains_key(&candidate)
                    .then_some(candidate)
            })
        })
    } else {
        None
    };
    let physical_name = physical_name.as_deref().unwrap_or(entry_name);
    let entry = p59_jar_lookup_entry(ctx, path, physical_name);
    let Ok(Value::Object(Some(physical_entry))) = entry else {
        return Ok(Some(entry?));
    };
    if physical_name == entry_name {
        return Ok(Some(Value::Object(Some(physical_entry))));
    }

    // Every allocation/re-entrant Java constructor below may move young
    // objects. Pin and reread all three references before constructing the
    // package-private JarFileEntry wrapper.
    let this_pin = ctx.pin_native_root(this);
    let entry_pin = ctx.pin_native_root(physical_entry);
    let logical_name = ctx.create_string(entry_name);
    let logical_pin = ctx.pin_native_root(logical_name);
    let this = ctx.read_native_pin(this_pin, this);
    let physical_entry = ctx.read_native_pin(entry_pin, physical_entry);
    let logical_name = ctx.read_native_pin(logical_pin, logical_name);
    let wrapped = ctx.new_object_initialized(
        "java/util/jar/JarFile$JarFileEntry",
        "(Ljava/util/jar/JarFile;Ljava/lang/String;Ljava/util/zip/ZipEntry;)V",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(logical_name)),
            Value::Object(Some(physical_entry)),
        ],
    );
    ctx.unpin_native_roots(logical_pin);
    ctx.unpin_native_roots(entry_pin);
    ctx.unpin_native_roots(this_pin);
    match wrapped? {
        Some(Value::Object(Some(wrapped))) => Ok(Some(Value::Object(Some(wrapped)))),
        _ => Ok(Some(Value::Object(Some(physical_entry)))),
    }
}

fn p59_jar_is_multi_release(path: &str) -> bool {
    let Some(contents) = jar_contents_cached(path) else {
        return false;
    };
    *contents
        .multi_release
        .get_or_init(|| p59_jar_manifest_declares_multi_release(path))
}

/// The actual manifest read behind `p59_jar_is_multi_release`. Runs at most
/// once per (jar, mtime) - see `JarContents::multi_release`.
fn p59_jar_manifest_declares_multi_release(path: &str) -> bool {
    let Some(bytes) = jar_entry_bytes_cached(path, "META-INF/MANIFEST.MF") else {
        return false;
    };
    let text = String::from_utf8_lossy(bytes.as_slice());
    let mut line = String::new();
    let mut matches_attribute = false;
    for raw_line in text.lines() {
        if raw_line.starts_with(' ') {
            line.push_str(raw_line.trim_start());
            continue;
        }
        if matches_attribute && line.trim().eq_ignore_ascii_case("Multi-Release: true") {
            return true;
        }
        line.clear();
        line.push_str(raw_line.trim_end_matches('\r'));
        matches_attribute = line
            .split_once(':')
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case("Multi-Release"));
    }
    matches_attribute && line.trim().eq_ignore_ascii_case("Multi-Release: true")
}

/// Look up a single entry by name in the JAR at `path`, returning a synthetic
/// JarEntry or `Value::Object(None)` if missing. Mirrors getEntry but
/// produces a `java/util/jar/JarEntry` (vs the older `java/util/zip/ZipEntry`).
pub(crate) fn p59_jar_lookup_entry(
    ctx: &mut dyn NativeContext,
    path: &str,
    entry_name: &str,
) -> Result<Value, MethodCallFailed> {
    if path.is_empty() || entry_name.is_empty() {
        return Ok(Value::Object(None));
    }
    let contents = match jar_contents_cached(path) {
        Some(c) => c,
        None => return Ok(Value::Object(None)),
    };
    let (name, size, csize, method, crc, comment, times) = match contents.by_name.get(entry_name) {
        Some(rec) => (
            entry_name.to_string(),
            rec.size,
            rec.csize,
            rec.method,
            rec.crc,
            rec.comment.clone(),
            rec.times,
        ),
        None => return Ok(Value::Object(None)),
    };
    let je = try_alloc_concurrent_synthetic(ctx, "java/util/jar/JarEntry", 5)?;
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh entry (native stale-local family).
    let je_pin = ctx.pin_native_root(je);
    let comment_s = comment.map(|comment| ctx.create_string(&comment));
    let comment_pin = comment_s.map(|comment| ctx.pin_native_root(comment));
    let name_s = ctx.create_string(&name);
    let je = ctx.read_native_pin(je_pin, je);
    let comment_s = comment_s.zip(comment_pin).map(|(comment, pin)| {
        let comment = ctx.read_native_pin(pin, comment);
        ctx.unpin_native_roots(pin);
        comment
    });
    ctx.unpin_native_roots(je_pin);
    ctx.set_field(je, 0, Value::Object(Some(name_s)));
    ctx.set_field(je, 1, Value::Long(size));
    ctx.set_field(je, 2, Value::Long(csize));
    ctx.set_field(je, 3, Value::Int(method));
    ctx.set_field(je, 4, Value::Object(comment_s));
    // Real-JDK mode: `ZipEntry.getSize()/getMethod()/getCompressedSize()/getCrc()`
    // run real bytecode reading the REAL fields by their actual offset, not the
    // synthetic slots above. Quarkus' RunnerClassLoader sizes its class-byte read
    // from `entry.getSize()`; a 0 there yields a 0-length class → ClassFormatError.
    // Mirror the real field names (matches zip_real_jar::alloc_zip_entry).
    ctx.set_field_by_name(je, "name", Value::Object(Some(name_s)));
    ctx.set_field_by_name(je, "size", Value::Long(size));
    ctx.set_field_by_name(je, "csize", Value::Long(csize));
    ctx.set_field_by_name(je, "method", Value::Int(method));
    ctx.set_field_by_name(je, "crc", Value::Long(crc));
    ctx.set_field_by_name(je, "comment", Value::Object(comment_s));
    p59_set_jar_entry_times(ctx, je, times);
    Ok(Value::Object(Some(je)))
}

pub(crate) fn p59_jar_file_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let path = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let elems = p59_jar_collect_entries(ctx, &path)?;
    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] JarFile.stream() path={:?} entries={}",
            path,
            elems.len()
        );
    }
    Ok(Some(Value::Object(Some(p56_build_stream(
        ctx,
        elems,
        "java/util/stream/Stream",
    )?))))
}

/// `BOOT-INF/classes/` plus `BOOT-INF/lib/*.jar` as `jar:nested:...` [`Value`]s
/// (SB3 `JarFileArchive` / launcher classpath scan).
pub(crate) fn p59_fat_jar_boot_inf_nested_url_values(
    ctx: &mut dyn NativeContext,
    jar_path: &str,
) -> Result<Vec<Value>, MethodCallFailed> {
    let mut urls: Vec<Value> = Vec::new();
    // Pin each fresh URL across the subsequent per-entry allocs — a moving
    // young GC there would relocate the earlier URLs (native stale-local
    // family). The pins stay live past this helper (the VM truncates the pin
    // vec at native exit); the refs are re-read right before returning.
    let mut pins: Vec<usize> = Vec::new();
    let jar_uri_path = jar_path.replace('\\', "/").replace('!', "%21");
    let classes_url_str = format!("jar:nested:/{jar_uri_path}/!BOOT-INF/classes/!/");
    let classes_url = p59_alloc_url(ctx, &classes_url_str)?;
    pins.push(ctx.pin_native_root(classes_url));
    urls.push(Value::Object(Some(classes_url)));
    if !jar_path.is_empty() {
        if let Ok(file) = std::fs::File::open(jar_path) {
            if let Ok(mut archive) = zip::ZipArchive::new(file) {
                for i in 0..archive.len() {
                    if let Ok(entry) = archive.by_index(i) {
                        let name = entry.name().to_string();
                        if name.starts_with("BOOT-INF/lib/") && name.ends_with(".jar") {
                            let url_str = format!("jar:nested:/{jar_uri_path}/!{name}!/");
                            let url = p59_alloc_url(ctx, &url_str)?;
                            pins.push(ctx.pin_native_root(url));
                            urls.push(Value::Object(Some(url)));
                        }
                    }
                }
            }
        }
    }
    // Re-read every URL to its current (post-GC) address before returning.
    for (v, h) in urls.iter_mut().zip(&pins) {
        if let Value::Object(Some(o)) = v {
            *v = Value::Object(Some(ctx.read_native_pin(*h, *o)));
        }
    }
    Ok(urls)
}

/// Spring Boot 3 launcher: read the JarFileArchive's `jarFile` field, walk
/// the central directory, apply Spring Boot's caller-supplied entry predicate,
/// and build a nested URL for every included entry.
///
/// Real-bytecode stream pipeline:
///   `jarFile.stream().map(JarArchiveEntry::new).filter(p1).map(this::getNestedJarUrl).collect(toCollection(LinkedHashSet::new))`
///
/// Replacing the stream pipeline with a native avoids the unsupported generic
/// `Stream.map / Stream.filter / Stream.collect` path while preserving the
/// source-level predicate contract. In particular, PropertiesLauncher uses
/// this with paths such as `app.jar!/` and must receive `foo.jar`, not an
/// unconditional `BOOT-INF/classes/` URL intended only for repackaged jars.
pub(crate) fn p59_spring_boot_jar_archive_get_class_path_urls(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let include_filter = obj_arg(args, 1)?;
    // JarFileArchive layout: field 0 = file (java.io.File), field 1 = jarFile.
    // Both `file_read_path` and JarFile field 0 store the path string, so
    // either route gets us the absolute path on disk.
    let file_obj = ctx.get_field(this, 0);
    let jar_path = match file_obj {
        Value::Object(Some(file_ref)) => {
            // File.field 0 = path String (set in register_phase57_file).
            match ctx.get_field(file_ref, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            }
        }
        _ => String::new(),
    };
    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] JarFileArchive.getClassPathUrls jar_path={:?}",
            jar_path
        );
    }

    let this_pin = ctx.pin_native_root(this);
    let include_filter_pin = ctx.pin_native_root(include_filter);
    let mut urls: Vec<(ObjectRef, usize)> = Vec::new();
    let jar_uri_path = jar_path.replace('\\', "/").replace('!', "%21");
    for jar_entry in p59_jar_collect_entries(ctx, &jar_path)? {
        let Value::Object(Some(jar_entry)) = jar_entry else {
            continue;
        };
        let jar_entry_pin = ctx.pin_native_root(jar_entry);
        let archive_entry = try_alloc_concurrent_synthetic(
            ctx,
            "org/springframework/boot/loader/launch/JarFileArchive$JarArchiveEntry",
            1,
        )?;
        let archive_entry_pin = ctx.pin_native_root(archive_entry);
        let jar_entry = ctx.read_native_pin(jar_entry_pin, jar_entry);
        let archive_entry = ctx.read_native_pin(archive_entry_pin, archive_entry);
        ctx.set_field(archive_entry, 0, Value::Object(Some(jar_entry)));
        ctx.set_field_by_name(archive_entry, "jarEntry", Value::Object(Some(jar_entry)));

        let include_filter = ctx.read_native_pin(include_filter_pin, include_filter);
        let archive_entry = ctx.read_native_pin(archive_entry_pin, archive_entry);
        let include = matches!(
            ctx.invoke_virtual(
                include_filter,
                "test",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(archive_entry))],
            )?,
            Some(Value::Int(value)) if value != 0
        );
        if include {
            let this = ctx.read_native_pin(this_pin, this);
            let archive_entry = ctx.read_native_pin(archive_entry_pin, archive_entry);
            let nested_url = ctx.invoke_special_bytecode_only(
                "org/springframework/boot/loader/launch/JarFileArchive",
                "getNestedJarUrl",
                "(Lorg/springframework/boot/loader/launch/JarFileArchive$JarArchiveEntry;)Ljava/net/URL;",
                &[Value::Object(Some(this)), Value::Object(Some(archive_entry))],
            )?;
            if let Some(Value::Object(Some(url))) = nested_url {
                urls.push((url, ctx.pin_native_root(url)));
            } else {
                let jar_entry = ctx.read_native_pin(jar_entry_pin, jar_entry);
                let name = match ctx.get_field_by_name(jar_entry, "name") {
                    Value::Object(Some(name)) => ctx.read_string(name).unwrap_or_default(),
                    _ => String::new(),
                };
                if !name.is_empty() {
                    // The Spring Boot nested protocol represents a directory
                    // class root (for example `BOOT-INF/classes/`) differently
                    // from a nested archive. The local class resolver understands
                    // the former `jar:nested:` form; preserve the ordinary
                    // `jar:file:` spelling for nested JAR/ZIP entries.
                    let url_text = format!("jar:nested:/{jar_uri_path}/!{name}!/");
                    let url = p59_alloc_url(ctx, &url_text)?;
                    urls.push((url, ctx.pin_native_root(url)));
                }
            }
        }
        ctx.unpin_native_roots(archive_entry_pin);
        ctx.unpin_native_roots(jar_entry_pin);
    }

    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] JarFileArchive.getClassPathUrls -> {} urls",
            urls.len()
        );
    }

    // This result is subsequently merged into a real LinkedHashSet by
    // PropertiesLauncher. A synthetic two-slot ArrayList only happens to work
    // for native collection consumers; real-JDK Collection.addAll walks the
    // actual inherited elementData/size fields and therefore saw it as empty.
    // Build a real ArrayList through its own native-backed constructor/add
    // path so both real bytecode and native callers observe the URLs.
    let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
        Some(Value::Object(Some(obj))) => obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let list_pin = ctx.pin_native_root(list);
    for (url, pin) in &urls {
        let list = ctx.read_native_pin(list_pin, list);
        let url = ctx.read_native_pin(*pin, *url);
        ctx.invoke_virtual(
            list,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(url))],
        )?;
    }
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    for (_, pin) in urls {
        ctx.unpin_native_roots(pin);
    }
    ctx.unpin_native_roots(include_filter_pin);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(list))))
}

/// Spring Boot 3 `ExplodedArchive.getClassPathUrls` without depending on the
/// real-JDK `LinkedList.addAll(0, ...)` path. The latter was retaining the
/// immediate directories but dropping every descendant under CratonVM, which
/// made a directory archive silently omit manifests, nested files, and names
/// requiring URI encoding.
pub(crate) fn p59_spring_boot_exploded_archive_get_class_path_urls(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let include_filter = obj_arg(args, 1)?;
    let directory_search_filter = obj_arg(args, 2)?;
    let root_directory = match ctx.get_field(this, 0) {
        Value::Object(Some(file)) => file_read_path(ctx, file),
        _ => String::new(),
    };
    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] ExplodedArchive.getClassPathUrls root_directory={:?}",
            root_directory
        );
    }
    let root_path = std::path::PathBuf::from(&root_directory);
    let mut pending = match std::fs::read_dir(&root_path) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    pending.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
    pending.reverse();

    let this_pin = ctx.pin_native_root(this);
    let include_filter_pin = ctx.pin_native_root(include_filter);
    let directory_search_filter_pin = ctx.pin_native_root(directory_search_filter);
    let mut urls = Vec::new();
    let mut url_pins = Vec::new();
    while let Some(path) = pending.pop() {
        let is_directory = path.is_dir();
        let Ok(relative) = path.strip_prefix(&root_path) else {
            continue;
        };
        let mut entry_name = relative.to_string_lossy().replace('\\', "/");
        if is_directory {
            entry_name.push('/');
        }
        let file = file_alloc(ctx, &path.to_string_lossy())?;
        let file_pin = ctx.pin_native_root(file);
        let archive_entry = try_alloc_concurrent_synthetic(
            ctx,
            "org/springframework/boot/loader/launch/ExplodedArchive$FileArchiveEntry",
            2,
        )?;
        let archive_entry_pin = ctx.pin_native_root(archive_entry);
        let name = ctx.create_string(&entry_name);
        let file = ctx.read_native_pin(file_pin, file);
        let archive_entry = ctx.read_native_pin(archive_entry_pin, archive_entry);
        ctx.set_field(archive_entry, 0, Value::Object(Some(name)));
        ctx.set_field(archive_entry, 1, Value::Object(Some(file)));
        ctx.set_field_by_name(archive_entry, "name", Value::Object(Some(name)));
        ctx.set_field_by_name(archive_entry, "file", Value::Object(Some(file)));

        if is_directory {
            let directory_search_filter =
                ctx.read_native_pin(directory_search_filter_pin, directory_search_filter);
            let archive_entry = ctx.read_native_pin(archive_entry_pin, archive_entry);
            let search = matches!(
                ctx.invoke_virtual(
                    directory_search_filter,
                    "test",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(archive_entry))],
                )?,
                Some(Value::Int(value)) if value != 0
            );
            if search {
                let mut children = match std::fs::read_dir(&path) {
                    Ok(entries) => entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .collect::<Vec<_>>(),
                    Err(_) => Vec::new(),
                };
                children.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
                children.reverse();
                pending.extend(children);
            }
        }

        let include_filter = ctx.read_native_pin(include_filter_pin, include_filter);
        let archive_entry = ctx.read_native_pin(archive_entry_pin, archive_entry);
        let include = matches!(
            ctx.invoke_virtual(
                include_filter,
                "test",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(archive_entry))],
            )?,
            Some(Value::Int(value)) if value != 0
        );
        if include {
            let file = ctx.read_native_pin(file_pin, file);
            let uri = ctx.invoke_virtual(file, "toURI", "()Ljava/net/URI;", &[])?;
            if let Some(Value::Object(Some(uri))) = uri {
                let uri_pin = ctx.pin_native_root(uri);
                let url = ctx.invoke_virtual(uri, "toURL", "()Ljava/net/URL;", &[])?;
                ctx.unpin_native_roots(uri_pin);
                if let Some(Value::Object(Some(url))) = url {
                    url_pins.push(ctx.pin_native_root(url));
                    urls.push(url);
                }
            }
        }
        ctx.unpin_native_roots(file_pin);
        ctx.unpin_native_roots(archive_entry_pin);
    }
    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] ExplodedArchive.getClassPathUrls -> {} urls",
            urls.len()
        );
    }

    // Unlike the JarFileArchive sibling above (whose result is only ever
    // consumed through a native `LinkedHashSet(Collection)` constructor
    // that reads a synthetic 2-field ArrayList's slots directly), this
    // return value is `.addAll()`'d into a real `LinkedHashSet` and
    // `new LinkedHashSet<>(...)`-copy-constructed by PropertiesLauncher's
    // own bytecode — both of which walk it via real bytecode's
    // `Collection.iterator()`. A hand-built synthetic ArrayList with
    // `elementData`/`size` hardcoded at slots 0/1 silently yields an empty
    // iteration in real-JDK mode, where those fields resolve to different
    // slots (inherited from AbstractList/AbstractCollection). Build a
    // genuine `ArrayList` through its own natively-backed `<init>`/`add`
    // so it stays correct regardless of the active field layout.
    let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
        Some(Value::Object(Some(obj))) => obj,
        _ => try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?,
    };
    let list_pin = ctx.pin_native_root(list);
    for (url, pin) in urls.iter().zip(&url_pins) {
        let list = ctx.read_native_pin(list_pin, list);
        let url = ctx.read_native_pin(*pin, *url);
        ctx.invoke_virtual(
            list,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(url))],
        )?;
    }
    let list = ctx.read_native_pin(list_pin, list);
    for pin in url_pins {
        ctx.unpin_native_roots(pin);
    }
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(include_filter_pin);
    ctx.unpin_native_roots(directory_search_filter_pin);
    ctx.unpin_native_roots(list_pin);
    Ok(Some(Value::Object(Some(list))))
}

/// Spring Boot 3 `ExecutableArchiveLauncher.createClassLoader(Collection)`:
/// real bytecode does `urls.toArray(new URL[0])` and can `ClassCastException`
/// when collection iteration / typed `toArray` does not match CratonVM's
/// mixed real-JDK + synthetic collection layout. Rebuild the nested-jar
/// `URL[]` from the fat-jar path (same scan as [`p59_fat_jar_boot_inf_nested_url_values`])
/// and `invokespecial` the private `Launcher.createClassLoader(URL[])` on
/// the real SB3 launcher type.
pub(crate) fn sb3_executable_archive_launcher_create_class_loader_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let urls = obj_arg(args, 1)?;
    let object_array = match ctx.invoke_virtual(urls, "toArray", "()[Ljava/lang/Object;", &[])? {
        Some(Value::Object(Some(arr))) => arr,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Pin across the URL scan / array alloc below — a moving young GC there
    // would relocate `this` and the collected URLs (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let object_array_pin = ctx.pin_native_root(object_array);
    let len = ctx.array_length(object_array);
    let indexed_array = match ctx.get_field_by_name(this, "classPathIndex") {
        Value::Object(Some(index)) => {
            match ctx.invoke_virtual(index, "getUrls", "()Ljava/util/List;", &[])? {
                Some(Value::Object(Some(list))) => {
                    match ctx.invoke_virtual(list, "toArray", "()[Ljava/lang/Object;", &[])? {
                        Some(Value::Object(Some(arr))) => Some(arr),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    };
    let indexed_array_pin = indexed_array.map(|arr| ctx.pin_native_root(arr));
    let indexed_len = indexed_array.map(|arr| ctx.array_length(arr)).unwrap_or(0);
    let url_arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Reference,
        len + indexed_len,
    );
    for i in 0..len {
        let object_array = ctx.read_native_pin(object_array_pin, object_array);
        let v = ctx.get_array_element(object_array, i);
        ctx.set_array_element(url_arr, i, v);
    }
    if let (Some(indexed_array), Some(indexed_array_pin)) = (indexed_array, indexed_array_pin) {
        for i in 0..indexed_len {
            let indexed_array = ctx.read_native_pin(indexed_array_pin, indexed_array);
            let v = ctx.get_array_element(indexed_array, i);
            ctx.set_array_element(url_arr, len + i, v);
        }
        ctx.unpin_native_roots(indexed_array_pin);
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(object_array_pin);
    ctx.invoke_special(
        "org/springframework/boot/loader/launch/Launcher",
        "createClassLoader",
        "([Ljava/net/URL;)Ljava/lang/ClassLoader;",
        &[Value::Object(Some(this)), Value::Object(Some(url_arr))],
    )
}

/// Resolve the on-disk fat-jar path of a Spring Boot 2 launcher instance.
///
/// The launcher's `this.archive` field is left null in our impl (its SB2
/// `JarFileArchive` ctor depends on plumbing we don't fully implement), so
/// instead of dereferencing it we re-derive the fat-jar path by asking the
/// classpath where the launcher's own subclass was loaded from. For
/// `--jar foo.jar` mode this yields `foo.jar`; for exploded layouts it
/// yields the directory root which the caller must filter out via
/// `isExploded()`.
pub(crate) fn sb2_launcher_jar_path(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<String> {
    let cid = ctx.class_id_of_object(this);
    let class_name = ctx.class_name_of_id(cid)?;
    ctx.find_class_source_path(&class_name)
}

/// `org/springframework/boot/loader/ExecutableArchiveLauncher.isExploded()`.
///
/// Spring Boot answers this with `this.archive.isExploded()`, i.e. "was the
/// application started from a directory rather than a packaged fat jar".
/// `this.archive` is null under CratonVM, but the same fact is directly
/// observable: [`sb2_launcher_jar_path`] resolves the launcher class's own
/// classpath source, which is the fat jar in packaged mode and the exploded
/// root directory otherwise. A directory there IS an exploded launch — the
/// flag reaches `LaunchedURLClassLoader`, which uses it to skip the
/// manifest-driven package-definition path that only applies to nested jars.
pub(crate) fn sb2_launcher_is_exploded(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let exploded = sb2_launcher_jar_path(ctx, this)
        .map(|p| std::path::Path::new(&p).is_dir())
        .unwrap_or(false);
    Ok(Some(Value::Int(i32::from(exploded))))
}

/// Read the Start-Class manifest entry from the fat-jar and return it as a
/// String. Falls back to "Main-Class" if Start-Class is absent (matching the
/// SB2 launcher's own behaviour on partially-formed manifests).
pub(crate) fn sb2_read_start_class(jar_path: &str) -> Option<String> {
    let file = std::fs::File::open(jar_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut entry = archive.by_name("META-INF/MANIFEST.MF").ok()?;
    use std::io::Read;
    let mut buf = String::new();
    entry.read_to_string(&mut buf).ok()?;
    let mut start_class: Option<String> = None;
    let mut main_class: Option<String> = None;
    // Manifest is line-oriented; continuation lines start with a single
    // space. Spring Boot only writes single-line attributes for these
    // keys so we skip continuation handling and just split on '\n'.
    for line in buf.lines() {
        if let Some(v) = line.strip_prefix("Start-Class: ") {
            start_class = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Main-Class: ") {
            main_class = Some(v.trim().to_string());
        }
    }
    start_class.or(main_class)
}

/// `org/springframework/boot/loader/ExecutableArchiveLauncher.getMainClass()`
/// — return the `Start-Class` manifest entry from the fat-jar directly,
/// bypassing the null `this.archive` field. Matches the SB2 launcher's
/// own contract: throws IllegalStateException when no Start-Class is
/// declared.
pub(crate) fn sb2_launcher_get_main_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let jar_path = sb2_launcher_jar_path(ctx, this).unwrap_or_default();
    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] SB2 ExecutableArchiveLauncher.getMainClass jar_path={:?}",
            jar_path
        );
    }
    let start_class = sb2_read_start_class(&jar_path);
    match start_class {
        Some(name) => Ok(Some(Value::Object(Some(ctx.create_string(&name))))),
        None => {
            // Spec-correct behaviour: throw IllegalStateException so callers
            // see the same surface as a real JDK + valid SB2 launcher.
            Err(RuntimeError::IllegalStateException {
                message: format!("No 'Start-Class' manifest entry specified in {}", jar_path),
            }
            .into())
        }
    }
}

/// Build a `JarFileArchive`-shaped result for SB2's `getClassPathArchives*`.
/// Each element of the returned List/Iterator is an SB2-style nested-archive
/// stand-in whose only required surface is `getUrl()` returning a `URL`.
/// We allocate `JarFileArchive` instances and pre-populate slot 1 (the `url`
/// field per SB2's instance layout) so the launcher's
/// `createClassLoader(List)` loop receives valid URLs.
pub(crate) fn sb2_launcher_build_archive_list(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let mut archives: Vec<Value> = Vec::new();
    let jar_path = match sb2_launcher_jar_path(ctx, this) {
        Some(p) => p,
        None => return Ok(archives),
    };
    let jar_uri_path = jar_path.replace('\\', "/").replace('!', "%21");

    // Always include BOOT-INF/classes/ as the first classpath URL (matches
    // SB2 JarLauncher.isNestedArchive's BOOT-INF/classes/ branch).
    let archive_class = "org/springframework/boot/loader/archive/JarFileArchive";
    let classes_url_str = format!("jar:file:/{jar_uri_path}!/BOOT-INF/classes!/");
    // Pin each fresh URL/archive across the subsequent per-entry allocs — a
    // moving young GC there would relocate the earlier ones (native
    // stale-local family). The pins stay live past this helper (the VM
    // truncates the pin vec at native exit); the refs are re-read right
    // before returning.
    let mut pins: Vec<usize> = Vec::new();
    let classes_url = p59_alloc_url(ctx, &classes_url_str)?;
    let classes_url_pin = ctx.pin_native_root(classes_url);
    let classes_archive = try_alloc_concurrent_synthetic(ctx, archive_class, 3)?;
    let classes_url = ctx.read_native_pin(classes_url_pin, classes_url);
    // SB2 JarFileArchive layout: 0=jarFile, 1=url, 2=tempUnpackDirectory
    ctx.set_field(classes_archive, 1, Value::Object(Some(classes_url)));
    pins.push(ctx.pin_native_root(classes_archive));
    archives.push(Value::Object(Some(classes_archive)));

    if let Ok(file) = std::fs::File::open(&jar_path) {
        if let Ok(mut zip) = zip::ZipArchive::new(file) {
            for i in 0..zip.len() {
                if let Ok(entry) = zip.by_index(i) {
                    let name = entry.name().to_string();
                    if name.starts_with("BOOT-INF/lib/") && name.ends_with(".jar") {
                        let url_str = format!("jar:file:/{jar_uri_path}!/{name}!/");
                        let url = p59_alloc_url(ctx, &url_str)?;
                        let url_pin = ctx.pin_native_root(url);
                        let nested = try_alloc_concurrent_synthetic(ctx, archive_class, 3)?;
                        let url = ctx.read_native_pin(url_pin, url);
                        ctx.set_field(nested, 1, Value::Object(Some(url)));
                        pins.push(ctx.pin_native_root(nested));
                        archives.push(Value::Object(Some(nested)));
                    }
                }
            }
        }
    }
    // Re-read every archive to its current (post-GC) address before returning.
    for (v, h) in archives.iter_mut().zip(&pins) {
        if let Value::Object(Some(o)) = v {
            *v = Value::Object(Some(ctx.read_native_pin(*h, *o)));
        }
    }
    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] SB2 ExecutableArchiveLauncher.getClassPathArchives -> {} entries",
            archives.len()
        );
    }
    Ok(archives)
}

/// `getClassPathArchives()` — SB2 v1 returns List<Archive>.
pub(crate) fn sb2_launcher_get_class_path_archives_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let archives = sb2_launcher_build_archive_list(ctx, this)?;
    // Pin the archives across the list/array allocs below — a moving young GC
    // there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &archives);
    let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let list_pin = ctx.pin_native_root(list);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, archives.len());
    let list = ctx.read_native_pin(list_pin, list);
    for (i, (v, p)) in archives.iter().zip(&pins).enumerate() {
        let v = read_pinned_object_value(ctx, *p, *v);
        ctx.set_array_element(arr, i, v);
    }
    ctx.set_field(list, 0, Value::Object(Some(arr)));
    ctx.set_field(list, 1, Value::Int(archives.len() as i32));
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    ctx.unpin_native_roots(first_pin.unwrap_or(list_pin));
    Ok(Some(Value::Object(Some(list))))
}

/// `getClassPathArchivesIterator()` — SB2 v2.3+ returns Iterator<Archive>.
/// Build the synthetic ArrayList of archives and return an `ArrayList$Itr`
/// iterator wired up by `native_collections`. The downstream
/// `Launcher.createClassLoader(Iterator)` only calls `Iterator.hasNext()`
/// / `Iterator.next()` which the existing collections-crate natives serve.
pub(crate) fn sb2_launcher_get_class_path_archives_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let archives = sb2_launcher_build_archive_list(ctx, this)?;
    // Wrap the archive array in `java/util/Enumeration$Impl` — a synthetic
    // class with `hasNext`/`next` natives already registered by
    // `register_enumeration_impl_natives` (layout: field 0 = elements
    // array, field 1 = cursor). This sidesteps the layout mismatch
    // between our synthetic `ArrayList$Itr` and real-JDK's
    // `ArrayList$Itr` (whose `cursor`/`this$0` fields sit at different
    // slots than our 2-field layout) — JDK bytecode for the iteration
    // loop dispatches `Iterator.hasNext()` via `invokeinterface`, which
    // our `try_stackless_invoke` resolves against the receiver's class:
    // `Enumeration$Impl` has the native registered, so the JDK bytecode
    // never runs.
    // Pin the archives across the array/iterator allocs below — a moving
    // young GC there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &archives);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, archives.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, (v, p)) in archives.iter().zip(&pins).enumerate() {
        let v = read_pinned_object_value(ctx, *p, *v);
        ctx.set_array_element(arr, i, v);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    let itr = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    ctx.unpin_native_roots(first_pin.unwrap_or(arr_pin));
    if crate::nbflags().dbg_sbload {
        eprintln!(
            "[DBG_SBLOAD] SB2 ExecutableArchiveLauncher.getClassPathArchivesIterator -> {} entries",
            archives.len()
        );
    }
    Ok(Some(Value::Object(Some(itr))))
}

/// Spring Boot 2 `Launcher.createClassLoader(Iterator<Archive>)` — bypass the
/// real bytecode loop that does `Iterator.next` + `checkcast Archive`. Mixed
/// CratonVM dispatch can leave the wrong reference on the stack so the cast
/// throws `ClassCastException` before `LaunchedURLClassLoader` is created.
/// Rebuild the `URL[]` from the same fat-jar scan as [`sb2_launcher_build_archive_list`]
/// and delegate to `createClassLoader([Ljava/net/URL;)`.
///
/// Also registered for `createClassLoader(List)` (Spring Boot 2.0.x — e.g.
/// SportMe) which uses the same iterator+checkcast loop over `List.iterator()`.
pub(crate) fn sb2_launcher_create_class_loader_bypass_archive_walk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let _ = args.get(1); // ignored — rebuilt from the launcher mirror
                         // Pin across the archive scan / URL[] alloc below — a moving young GC
                         // there would relocate them (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let archives = sb2_launcher_build_archive_list(ctx, this)?;
    let mut urls: Vec<ObjectRef> = Vec::with_capacity(archives.len());
    let mut url_pins: Vec<usize> = Vec::with_capacity(archives.len());
    for arch_val in archives {
        let Value::Object(Some(arch_obj)) = arch_val else {
            continue;
        };
        if let Value::Object(Some(url)) = ctx.get_field(arch_obj, 1) {
            url_pins.push(ctx.pin_native_root(url));
            urls.push(url);
        }
    }
    let url_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    for (i, u) in urls.iter().enumerate() {
        let u = ctx.read_native_pin(url_pins[i], *u);
        ctx.set_array_element(url_arr, i, Value::Object(Some(u)));
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.invoke(
        "org/springframework/boot/loader/Launcher",
        "createClassLoader",
        "([Ljava/net/URL;)Ljava/lang/ClassLoader;",
        &[Value::Object(Some(this)), Value::Object(Some(url_arr))],
    )
}

/// Allocate a 13-field synthetic URL with `protocol`, `host`, `port`, `file`,
/// `path`, and `full` populated so URL.toString / URL.toURI / URL.getPath
/// all return the right thing for downstream classpath consumers.
pub(crate) fn p59_alloc_url(
    ctx: &mut dyn NativeContext,
    full: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let url = try_alloc_concurrent_synthetic(ctx, "java/net/URL", 13)?;
    let proto = if let Some(idx) = full.find(':') {
        &full[..idx]
    } else {
        ""
    };
    let rest = if proto.is_empty() {
        full
    } else {
        &full[proto.len() + 1..]
    };
    // Pin across the create_strings below — a moving young GC there would
    // relocate the fresh URL and the earlier strings (native stale-local
    // family).
    let url_pin = ctx.pin_native_root(url);
    let proto_s = ctx.create_string(proto);
    let proto_pin = ctx.pin_native_root(proto_s);
    let file_s = ctx.create_string(rest);
    let file_pin = ctx.pin_native_root(file_s);
    let full_s = ctx.create_string(full);
    let full_pin = ctx.pin_native_root(full_s);
    let host_s = ctx.create_string("");
    let url = ctx.read_native_pin(url_pin, url);
    let proto_s = ctx.read_native_pin(proto_pin, proto_s);
    let file_s = ctx.read_native_pin(file_pin, file_s);
    let full_s = ctx.read_native_pin(full_pin, full_s);
    ctx.set_field(url, 0, Value::Object(Some(proto_s))); // protocol
    ctx.set_field(url, 1, Value::Object(Some(host_s))); // host
    ctx.set_field(url, 2, Value::Int(-1)); // port
    ctx.set_field(url, 3, Value::Object(Some(file_s))); // file
    ctx.set_field(url, 4, Value::Object(None)); // query
                                                // Slot 5 = authority — leave null to satisfy our URL.toString fallback.
    ctx.set_field(url, 5, Value::Object(Some(full_s))); // authority/full
    ctx.set_field(url, 6, Value::Object(Some(file_s))); // path
    ctx.unpin_native_roots(url_pin);
    Ok(url)
}

pub(crate) fn p59_jar_file_entries(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let path = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let elems = p59_jar_collect_entries(ctx, &path)?;
    // Pack into the concrete synthetic `Enumeration$Impl` (array=0,
    // cursor=1). Allocating the bare `java/util/Enumeration` interface
    // produced an object with no instantiable concrete class — it degraded
    // to `java/lang/Object` and `invokeinterface hasMoreElements` failed.
    // Pin the entries across the array/Enumeration allocs below — a moving
    // young GC there would relocate them (native stale-local family).
    let pins = pin_object_values(ctx, &elems);
    let first_pin = pins.iter().flatten().next().map(|(h, _)| *h);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, elems.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, (v, p)) in elems.iter().zip(&pins).enumerate() {
        let v = read_pinned_object_value(ctx, *p, *v);
        ctx.set_array_element(arr, i, v);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    let enumeration = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    ctx.unpin_native_roots(first_pin.unwrap_or(arr_pin));
    Ok(Some(Value::Object(Some(enumeration))))
}

// JarFile = 2-field (name=0 String, manifest=1)
// Real implementation reads MANIFEST.MF from the JAR file using zip crate.
pub(crate) fn p59_jar_file_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name_val = args.get(1).copied().unwrap_or(Value::Object(None));
    // HIB-CV-17: normalize a leading-slash Windows drive path (`/C:/foo` →
    // `C:/foo`) BEFORE storing it in slot 0. Every reader — `getEntry`,
    // `getInputStream`, `entries`, `getManifest` — `File::open`s the slot-0
    // path, and on Windows `File::open("/C:/foo")` opens an empty/wrong target
    // (zero entries). `new JarFile(url.toURI().getSchemeSpecificPart())` yields
    // exactly the `/C:/...` form on Windows, so Hibernate's packaged-`.par`
    // scan found no `META-INF/persistence.xml`. The `JarFile(File)` ctor was
    // unaffected because `java.io.File` already normalizes the drive path.
    // Storing the normalized form also makes `getName()` match HotSpot's
    // `file.getPath()`. `p57_to_os_path` is a no-op for already-normal paths.
    // Pin across the create_string / manifest parse below — a moving young GC
    // there would relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let (slot0_val, path) = match name_val {
        Value::Object(Some(s)) => {
            let raw = ctx.read_string(s).unwrap_or_default();
            let norm = p57_to_os_path(&raw);
            if norm != raw {
                let ns = ctx.create_string(&norm);
                (Value::Object(Some(ns)), norm)
            } else {
                (name_val, raw)
            }
        }
        _ => (name_val, String::new()),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 0, slot0_val);
    ctx.set_field_by_name(this, "name", slot0_val);
    // Opening is the moment the jar caches re-check the file on disk; the
    // accessors read a memoised mtime. See `jar_path_mtime`.
    jar_cache_revalidate(&path);
    // Keep signed entry sections lazy. Constructing a JarFile only needs main
    // attributes; expanding thousands of signer entries here makes ordinary
    // construction pathological in the interpreter.
    let manifest = p98_read_jar_manifest_main(ctx, &path);
    let manifest_pin = match manifest {
        Ok(Value::Object(Some(m))) => Some((ctx.pin_native_root(m), m)),
        _ => None,
    };
    let manifest_ref = if let Some((pin, fallback)) = manifest_pin {
        let manifest_obj = ctx.read_native_pin(pin, fallback);
        match ctx.new_object_initialized(
            "java/lang/ref/SoftReference",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(manifest_obj))],
        ) {
            Ok(Some(Value::Object(Some(sr)))) => {
                let manifest_obj = ctx.read_native_pin(pin, fallback);
                ctx.set_field_by_name(sr, "referent", Value::Object(Some(manifest_obj)));
                Value::Object(Some(sr))
            }
            _ => Value::Object(None),
        }
    } else {
        Value::Object(None)
    };
    let this = ctx.read_native_pin(this_pin, this);
    let manifest = match manifest_pin {
        Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
        None => manifest?,
    };
    ctx.set_field(this, 1, manifest);
    ctx.set_field_by_name(this, "manRef", manifest_ref);
    if let Some((pin, _)) = manifest_pin {
        ctx.unpin_native_roots(pin);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p59_jar_file_init_file(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let path_val = if let Some(Value::Object(Some(file))) = args.get(1) {
        ctx.get_field(*file, 0)
    } else {
        Value::Object(None)
    };
    ctx.set_field(this, 0, path_val);
    ctx.set_field_by_name(this, "name", path_val);
    let path = if let Value::Object(Some(s)) = path_val {
        ctx.read_string(s).unwrap_or_default()
    } else {
        String::new()
    };
    if crate::nbflags().dbg_sbload {
        eprintln!("[DBG_SBLOAD] JarFile.<init>(File) path={:?}", path);
    }
    // Opening is the moment the jar caches re-check the file on disk; the
    // accessors read a memoised mtime. See `jar_path_mtime`.
    jar_cache_revalidate(&path);
    // Pin across the manifest parse below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    // Keep signed entry sections lazy, as in the String constructor above.
    let manifest = p98_read_jar_manifest_main(ctx, &path);
    let manifest_pin = match manifest {
        Ok(Value::Object(Some(m))) => Some((ctx.pin_native_root(m), m)),
        _ => None,
    };
    let manifest_ref = if let Some((pin, fallback)) = manifest_pin {
        let manifest_obj = ctx.read_native_pin(pin, fallback);
        match ctx.new_object_initialized(
            "java/lang/ref/SoftReference",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(manifest_obj))],
        ) {
            Ok(Some(Value::Object(Some(sr)))) => {
                let manifest_obj = ctx.read_native_pin(pin, fallback);
                ctx.set_field_by_name(sr, "referent", Value::Object(Some(manifest_obj)));
                Value::Object(Some(sr))
            }
            _ => Value::Object(None),
        }
    } else {
        Value::Object(None)
    };
    let this = ctx.read_native_pin(this_pin, this);
    let manifest = match manifest_pin {
        Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
        None => manifest?,
    };
    ctx.set_field(this, 1, manifest);
    ctx.set_field_by_name(this, "manRef", manifest_ref);
    if let Some((pin, _)) = manifest_pin {
        ctx.unpin_native_roots(pin);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p59_jar_file_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => match ctx.get_field_by_name(this, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
    }
}

pub(crate) fn p59_jar_file_manifest(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let path = p59_jar_file_path(ctx, this);
    Ok(Some(p98_read_jar_manifest(ctx, &path)?))
}

/// Read MANIFEST.MF from a JAR file and create a Manifest synthetic object.
///
/// PERF (2026-07-23): this used to `File::open` + `zip::ZipArchive::new` the
/// WHOLE jar itself, independent of (and redundant with)
/// `jar_contents_cached`/`jar_entry_bytes_cached`'s caches — every single
/// `new JarFile(path)` (this runs from every `<init>` handler) re-parsed the
/// entire central directory again just to grab one entry. For a jar the size
/// of testcontainers.jar (12.5k entries, ~17MB) that central-directory parse
/// alone measured ~105ms; Spring Boot test suites that construct many
/// short-lived JarFile/classloader instances over the run (one context
/// refresh per `@Test` method, `ApplicationContextRunner`, etc.) pay that
/// cost again on every construction. Route through the same lazily-cached
/// `jar_entry_bytes_cached` the `getInputStream` native uses so only the
/// FIRST touch of a given (path, mtime) pays for the archive open.
pub(crate) fn p98_read_jar_manifest(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<Value, MethodCallFailed> {
    Ok(p98_read_jar_manifest_impl(ctx, path, true)?)
}

/// Constructor variant of [`p98_read_jar_manifest`]. The main attributes are
/// sufficient during JarFile construction; signed entry sections are expanded
/// only when a caller asks for the full manifest.
pub(crate) fn p98_read_jar_manifest_main(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<Value, MethodCallFailed> {
    Ok(p98_read_jar_manifest_impl(ctx, path, false)?)
}

fn p98_read_jar_manifest_impl(
    ctx: &mut dyn NativeContext,
    path: &str,
    include_entries: bool,
) -> Result<Value, MethodCallFailed> {
    if path.is_empty() {
        return Ok(Value::Object(None));
    }
    let manifest_bytes = match jar_entry_bytes_cached(path, "META-INF/MANIFEST.MF") {
        Some(b) => (*b).clone(),
        None => return Ok(Value::Object(None)),
    };
    // Parse the main section, including folded continuation lines, through the
    // same manifest parser used by the `Manifest(InputStream)` bridge. Keeping
    // one parser prevents JarFile.getManifest() from drifting from the normal
    // constructor path on long attributes such as Spring Boot's Class-Path.
    // Build a REAL Manifest whose Attributes is backed by a real map —
    // consistent with getValue/putValue/size/write (see
    // p59_manifest_new_attributes). The previous synthetic 3-slot Attributes
    // was wrong for the 1-field real layout and read back null/empty.
    // Parse all sections, including signed-jar `Name:` entry attributes,
    // then build a REAL Manifest whose Attributes is backed by a real map —
    // consistent with getValue/putValue/size/write (see
    // p59_manifest_new_attributes). The previous synthetic 3-slot Attributes
    // was wrong for the 1-field real layout and read back null/empty.
    let parsed = match if include_entries {
        p59_parse_manifest_bytes(&manifest_bytes)
    } else {
        p59_parse_manifest_main_bytes(&manifest_bytes)
    } {
        Ok(parsed) => parsed,
        Err(_) => return Ok(Value::Object(None)),
    };
    // A real Manifest (its <init> native installs a real empty Attributes at
    // slot 0 and a real entries map at slot 1); populate both maps through
    // real bytecode. Pin across the allocating calls.
    let manifest = match ctx.new_object_initialized("java/util/jar/Manifest", "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Ok(Value::Object(None)),
    };
    let man_pin = ctx.pin_native_root(manifest);
    let attrs = match ctx.get_field(manifest, 0) {
        Value::Object(Some(a)) => a,
        _ => match ctx.get_field_by_name(manifest, "attr") {
            Value::Object(Some(a)) => a,
            _ => {
                ctx.unpin_native_roots(man_pin);
                return Ok(Value::Object(None));
            }
        },
    };
    let attrs_pin = ctx.pin_native_root(attrs);
    if p59_attrs_populate_real(ctx, attrs_pin, attrs, &parsed.main).is_err() {
        ctx.unpin_native_roots(man_pin);
        return Ok(Value::Object(None));
    }
    let manifest = ctx.read_native_pin(man_pin, manifest);
    let entries_map = match ctx.get_field(manifest, 1) {
        Value::Object(Some(entries)) => entries,
        _ => match ctx.get_field_by_name(manifest, "entries") {
            Value::Object(Some(entries)) => entries,
            _ => {
                ctx.unpin_native_roots(man_pin);
                return Ok(Value::Object(None));
            }
        },
    };
    if include_entries {
        let entries_pin = ctx.pin_native_root(entries_map);
        for (name, pairs) in &parsed.entries {
            let entry_attrs = p59_manifest_new_attributes(ctx)?;
            let entry_pin = ctx.pin_native_root(entry_attrs);
            if p59_attrs_populate_real(ctx, entry_pin, entry_attrs, pairs).is_err() {
                ctx.unpin_native_roots(man_pin);
                return Ok(Value::Object(None));
            }
            let name = ctx.create_string(name);
            let name_pin = ctx.pin_native_root(name);
            let entries_map = ctx.read_native_pin(entries_pin, entries_map);
            let entry_attrs = ctx.read_native_pin(entry_pin, entry_attrs);
            let name = ctx.read_native_pin(name_pin, name);
            if ctx
                .invoke(
                    "java/util/LinkedHashMap",
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[
                        Value::Object(Some(entries_map)),
                        Value::Object(Some(name)),
                        Value::Object(Some(entry_attrs)),
                    ],
                )
                .is_err()
            {
                ctx.unpin_native_roots(man_pin);
                return Ok(Value::Object(None));
            }
        }
    }
    let manifest = ctx.read_native_pin(man_pin, manifest);
    ctx.unpin_native_roots(man_pin);
    Ok(Value::Object(Some(manifest)))
}

// =============================================================================
// Manifest(InputStream) synthetic — parse a MANIFEST.MF byte stream and
// populate the Manifest object's mainAttributes / entries fields. Mirrors
// java.util.jar.Manifest(InputStream) for synthetic-jdk mode so that
// JarFile.getManifest() (native-io/src/zip_real_jar.rs) can succeed when
// the real-JDK bytecode for Manifest isn't loaded.
// =============================================================================

/// Allocate a fresh synthetic `java.util.jar.Attributes` with an empty
/// buckets array (alternating key/value Strings; fixed cap=64 for simplicity).
pub(crate) fn p59_manifest_new_attributes(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    // Build a REAL java.util.jar.Attributes: its single `map` field is a real
    // LinkedHashMap, so every access path — Map.put/entrySet (real bytecode),
    // getValue/putValue/size (also real bytecode now) and Manifest.write's
    // writeMain — operates on one consistent backing. The previous synthetic
    // 3-slot layout (buckets/size/capacity) was wrong: Attributes has exactly
    // one field, so slots 1/2 were out-of-bounds and slot 0 (the real `map`)
    // was misread as a bucket array. (Out-of-bounds slot writes used to land
    // on adjacent memory and "work" by accident until the gen_heap OOB guard
    // started dropping them, which made every Manifest attribute read null.)
    if let Ok(Some(Value::Object(Some(o)))) =
        ctx.new_object_initialized("java/util/jar/Attributes", "()V", &[])
    {
        return Ok(o);
    }
    // Fallback: a bare allocation (map left at its default) is still better
    // than the broken synthetic layout.
    Ok(try_alloc_concurrent_synthetic(
        ctx,
        "java/util/jar/Attributes",
        1,
    )?)
}

/// Allocate a fresh HashMap-shaped synthetic for per-entry Attributes
/// (keys = String entry names, values = Attributes). Uses the same
/// alternating-pair layout in a single flat Object[] bucket (the rest
/// of the codebase already mixes the real HashMap-node layout with this
/// pair-list one, and `getEntries()` callers only walk the entries via
/// their own iteration so the layout choice is local).
pub(crate) fn p59_manifest_new_entries_map(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    // A REAL LinkedHashMap (name -> Attributes), for the same reason as
    // p59_manifest_new_attributes: the synthetic 3-slot layout did not match
    // java.util.HashMap's real field layout, so real Map ops (put/get/entrySet,
    // used by Manifest.write and getEntries consumers) saw a different/empty
    // backing than the synthetic insert path.
    if let Ok(Some(Value::Object(Some(o)))) =
        ctx.new_object_initialized("java/util/LinkedHashMap", "()V", &[])
    {
        return Ok(o);
    }
    Ok(try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 1)?)
}

/// Read all bytes from an InputStream. For a synthetic ByteArrayInputStream
/// (the common case — see native-io/src/zip_real_jar.rs which always
/// wraps the manifest bytes in a BAIS) we drop down to the known 4-field
/// layout and bulk-copy the backing byte array. For any other InputStream
/// subtype we fall back to invoke_virtual("read()I") in a loop.
pub(crate) fn p59_read_input_stream_fully(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    // Fast path: if the class name reports ByteArrayInputStream, read the
    // backing array directly.
    let cid = ctx.class_id_of_object(stream);
    let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
    if class_name == "java/io/ByteArrayInputStream" {
        // Layout: buf=0 ([B), pos=1 (I), mark=2 (I), count=3 (I).
        let pos = match ctx.get_field_by_name(stream, "pos") {
            Value::Int(v) => v.max(0) as usize,
            _ => match ctx.get_field(stream, 1) {
                Value::Int(v) => v.max(0) as usize,
                _ => 0,
            },
        };
        let count = match ctx.get_field_by_name(stream, "count") {
            Value::Int(v) => v.max(0) as usize,
            _ => match ctx.get_field(stream, 3) {
                Value::Int(v) => v.max(0) as usize,
                _ => 0,
            },
        };
        let buf = match ctx.get_field_by_name(stream, "buf") {
            Value::Object(Some(arr)) => Value::Object(Some(arr)),
            _ => ctx.get_field(stream, 0),
        };
        if let Value::Object(Some(arr)) = buf {
            let arr_len = ctx.array_length(arr);
            let end = count.min(arr_len);
            if pos >= end {
                return Ok(Vec::new());
            }
            let mut out = Vec::with_capacity(end - pos);
            for i in pos..end {
                let b = match ctx.get_array_element(arr, i) {
                    Value::Int(v) => v as u8,
                    _ => 0,
                };
                out.push(b);
            }
            // Advance pos to count so subsequent reads see EOF, mirroring
            // a real stream's consumed state.
            ctx.set_field(stream, 1, Value::Int(end as i32));
            return Ok(out);
        }
        return Ok(Vec::new());
    }
    // Slow path: loop on invoke_virtual("read()I") until -1.
    // Pin across the read callbacks below — a moving young GC there would
    // relocate the stream (native stale-local family).
    let stream_pin = ctx.pin_native_root(stream);
    let mut out = Vec::new();
    loop {
        let stream = ctx.read_native_pin(stream_pin, stream);
        let r = match ctx.invoke_virtual(stream, "read", "()I", &[]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(stream_pin);
                return Err(e);
            }
        };
        match r {
            Some(Value::Int(-1)) => break,
            Some(Value::Int(v)) => out.push((v & 0xff) as u8),
            None | Some(Value::Object(None)) => break,
            Some(other) => {
                ctx.unpin_native_roots(stream_pin);
                return Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(RuntimeError::IOException {
                        message: format!(
                            "Manifest(InputStream): unexpected read() return {:?}",
                            other
                        ),
                    }),
                ));
            }
        }
        // Guard against runaway streams (e.g., buggy read() that never
        // returns -1). Manifest files are tiny — 1 MB is plenty of headroom.
        if out.len() > 1024 * 1024 {
            ctx.unpin_native_roots(stream_pin);
            return Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(RuntimeError::IOException {
                    message: "Manifest(InputStream): stream exceeds 1 MB limit".to_string(),
                }),
            ));
        }
    }
    ctx.unpin_native_roots(stream_pin);
    Ok(out)
}

/// Parse only a manifest's main section for JarFile construction.
///
/// The full parser is intentionally lazy: signed archives often carry a
/// `Name:` section for every class, and a constructor only needs the main
/// attributes (notably `Multi-Release`).
fn p59_parse_manifest_main_bytes(data: &[u8]) -> Result<ParsedManifest, String> {
    let text = String::from_utf8_lossy(data);
    let mut logical: Vec<String> = Vec::new();
    for raw in text.lines() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.is_empty() {
            break;
        }
        if let Some(continuation) = raw.strip_prefix(' ') {
            let previous = logical
                .last_mut()
                .ok_or_else(|| "continuation line with no predecessor".to_string())?;
            previous.push_str(continuation);
        } else {
            logical.push(raw.to_string());
        }
    }
    let mut main = Vec::with_capacity(logical.len());
    for line in logical {
        let colon = line
            .find(':')
            .ok_or_else(|| format!("malformed manifest line: {line}"))?;
        let key = line[..colon].trim().to_string();
        if key.is_empty() {
            return Err(format!("empty manifest key in line: {line}"));
        }
        let rest = &line[colon + 1..];
        let value = rest
            .strip_prefix(' ')
            .unwrap_or(rest)
            .trim_end()
            .to_string();
        main.push((key, value));
    }
    Ok(ParsedManifest {
        main,
        entries: Vec::new(),
    })
}

/// Parse a MANIFEST.MF byte buffer into (main_attrs, Vec<(entry_name,
/// entry_attrs)>). Handles CRLF/LF, continuation lines (leading space
/// appends to previous value), blank-line section separators, trailing
/// whitespace stripping, and the "Name: <path>" entry-section marker.
pub(crate) fn p59_parse_manifest_bytes(data: &[u8]) -> Result<ParsedManifest, String> {
    // Convert to a String, tolerating non-UTF8 by replacing invalid bytes —
    // manifests are spec-required UTF-8 but we don't want to panic on
    // synthetic-test inputs with stray bytes.
    let text = String::from_utf8_lossy(data);

    // Split into physical lines (handle CRLF, LF, and mixed input).
    let lines: Vec<String> = {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\r' {
                // Consume optional trailing \n.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            } else if c == '\n' {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    };

    // Collapse continuation lines (lines beginning with a single space are
    // appended to the prior logical line, with the leading space stripped).
    let mut logical: Vec<String> = Vec::new();
    for raw in lines.into_iter() {
        if raw.starts_with(' ') {
            if let Some(prev) = logical.last_mut() {
                prev.push_str(&raw[1..]);
                continue;
            }
            // Continuation with no predecessor — treat as malformed.
            return Err("continuation line with no predecessor".to_string());
        }
        logical.push(raw);
    }

    // Split into sections by blank logical lines.
    let mut sections: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for line in logical.into_iter() {
        if line.is_empty() {
            if !cur.is_empty() {
                sections.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(line);
        }
    }
    if !cur.is_empty() {
        sections.push(cur);
    }

    // Parse each section into Vec<(key, value)>.
    fn parse_section(section: &[String]) -> Result<Vec<(String, String)>, String> {
        let mut pairs = Vec::new();
        for line in section {
            let colon = match line.find(':') {
                Some(idx) => idx,
                None => return Err(format!("malformed manifest line: {}", line)),
            };
            let key = line[..colon].trim().to_string();
            // Spec: the colon is followed by a space; be lenient and accept
            // "key:value" too.
            let rest = &line[colon + 1..];
            let value = rest
                .strip_prefix(' ')
                .unwrap_or(rest)
                .trim_end()
                .to_string();
            if key.is_empty() {
                return Err(format!("empty manifest key in line: {}", line));
            }
            pairs.push((key, value));
        }
        Ok(pairs)
    }

    let mut parsed_sections: Vec<Vec<(String, String)>> = Vec::with_capacity(sections.len());
    for section in sections {
        parsed_sections.push(parse_section(&section)?);
    }
    // `Manifest.write()` may emit an empty main section followed immediately
    // by a `Name:` section when a manifest contains only per-entry attributes.
    // Preserve that first section as an entry rather than mistaking it for the
    // main attributes; signed-library detection relies on exactly this shape.
    let first_is_entry = parsed_sections.first().is_some_and(|pairs| {
        pairs
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("Name"))
    });
    let main = if first_is_entry || parsed_sections.is_empty() {
        Vec::new()
    } else {
        parsed_sections.remove(0)
    };
    let mut entries: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for pairs in parsed_sections {
        // Spec: entry sections start with a `Name: <path>` line.
        let name = pairs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Name"))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| "entry section missing Name attribute".to_string())?;
        let rest: Vec<(String, String)> = pairs
            .into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("Name"))
            .collect();
        entries.push((name, rest));
    }
    Ok(ParsedManifest { main, entries })
}

pub(crate) struct ParsedManifest {
    main: Vec<(String, String)>,
    entries: Vec<(String, Vec<(String, String)>)>,
}

#[cfg(test)]
pub(crate) mod manifest_parser_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn parses_signed_jar_entry_sections_after_main_attributes() {
        let parsed = p59_parse_manifest_bytes(
            b"Manifest-Version: 1.0\r\nCreated-By: CratonVM\r\n\r\nName: com/example/App.class\r\nSHA-256-Digest: abc\r\n def\r\n\r\nName: META-INF/services/example\r\nSHA-512-Digest: ghi\r\n\r\n",
        )
        .expect("valid signed-jar manifest");

        assert_eq!(
            parsed.main,
            vec![
                ("Manifest-Version".into(), "1.0".into()),
                ("Created-By".into(), "CratonVM".into())
            ]
        );
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].0, "com/example/App.class");
        assert_eq!(
            parsed.entries[0].1,
            vec![("SHA-256-Digest".into(), "abcdef".into())]
        );
        assert_eq!(parsed.entries[1].0, "META-INF/services/example");
        assert_eq!(
            parsed.entries[1].1,
            vec![("SHA-512-Digest".into(), "ghi".into())]
        );

        let entry_only =
            p59_parse_manifest_bytes(b"\r\nName: a/b/C.class\r\nSHA1-Digest: 0000\r\n\r\n")
                .expect("valid manifest with an empty main section");
        assert!(entry_only.main.is_empty());
        assert_eq!(
            entry_only.entries,
            vec![(
                "a/b/C.class".into(),
                vec![("SHA1-Digest".into(), "0000".into())],
            )]
        );
    }
}

/// Synthetic `java.util.jar.Manifest.<init>(InputStream)`. Reads the full
/// stream, parses it as a MANIFEST.MF, and installs the resulting
/// mainAttributes / entries onto `this`. On any IO or parse error throws
/// `java.io.IOException` rather than panicking.
pub(crate) fn p59_manifest_init_from_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    p59_manifest_init_from_input_stream_at(ctx, args, 1)
}

pub(crate) fn p59_manifest_init_from_verified_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    p59_manifest_init_from_input_stream_at(ctx, args, 2)
}

pub(crate) fn p59_manifest_init_copy(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let source = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let source_field =
        |ctx: &mut dyn NativeContext, obj: ObjectRef, name: &str, slot: usize| match ctx
            .get_field_by_name(obj, name)
        {
            Value::Object(Some(o)) => Some(o),
            _ => match ctx.get_field(obj, slot) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            },
        };

    // `Manifest(Manifest)` is a copy constructor, not a view constructor.
    // Spring Boot removes launcher-only keys from a copy before reading the
    // original `Start-Class`, so sharing Attributes loses that source value.
    let this_pin = ctx.pin_native_root(this);
    let attrs = match source.and_then(|src| source_field(ctx, src, "attr", 0)) {
        Some(source_attrs) => {
            let source_pin = ctx.pin_native_root(source_attrs);
            let source_attrs = ctx.read_native_pin(source_pin, source_attrs);
            let copied = ctx.new_object_initialized(
                "java/util/jar/Attributes",
                "(Ljava/util/jar/Attributes;)V",
                &[Value::Object(Some(source_attrs))],
            )?;
            ctx.unpin_native_roots(source_pin);
            match copied {
                Some(Value::Object(Some(attrs))) => attrs,
                _ => p59_manifest_new_attributes(ctx)?,
            }
        }
        None => p59_manifest_new_attributes(ctx)?,
    };
    let attrs_pin = ctx.pin_native_root(attrs);
    let entries = match source.and_then(|src| source_field(ctx, src, "entries", 1)) {
        Some(source_entries) => {
            let source_pin = ctx.pin_native_root(source_entries);
            let source_entries = ctx.read_native_pin(source_pin, source_entries);
            let copied = ctx.new_object_initialized(
                "java/util/LinkedHashMap",
                "(Ljava/util/Map;)V",
                &[Value::Object(Some(source_entries))],
            )?;
            ctx.unpin_native_roots(source_pin);
            match copied {
                Some(Value::Object(Some(entries))) => entries,
                _ => p59_manifest_new_entries_map(ctx)?,
            }
        }
        None => p59_manifest_new_entries_map(ctx)?,
    };
    let entries_pin = ctx.pin_native_root(entries);

    let this = ctx.read_native_pin(this_pin, this);
    let attrs = ctx.read_native_pin(attrs_pin, attrs);
    let entries = ctx.read_native_pin(entries_pin, entries);
    ctx.set_field(this, 0, Value::Object(Some(attrs)));
    ctx.set_field(this, 1, Value::Object(Some(entries)));
    ctx.set_field_by_name(this, "attr", Value::Object(Some(attrs)));
    ctx.set_field_by_name(this, "entries", Value::Object(Some(entries)));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p59_manifest_init_from_input_stream_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    stream_index: usize,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let stream = match args.get(stream_index) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(RuntimeError::NullPointerException {
                    message: Some("Manifest(InputStream): stream is null".to_string()),
                }),
            ));
        }
    };
    // Pin the Manifest across the whole parse: reading the stream may invoke
    // Java, and building the real Attributes / entries map allocates — any of
    // which can move objects under the collector. `unpin_native_roots(this_pin)`
    // at the end frees this pin and every pin taken after it.
    let this_pin = ctx.pin_native_root(this);
    let result = (|| -> MethodCallResult {
        let bytes = p59_read_input_stream_fully(ctx, stream)?;
        let parsed = match p59_parse_manifest_bytes(&bytes) {
            Ok(p) => p,
            Err(msg) => {
                return Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(RuntimeError::IOException {
                        message: format!("invalid manifest: {}", msg),
                    }),
                ));
            }
        };

        // Main attributes: a real Attributes populated through real putValue.
        let main_attrs = p59_manifest_new_attributes(ctx)?;
        let main_pin = ctx.pin_native_root(main_attrs);
        p59_attrs_populate_real(ctx, main_pin, main_attrs, &parsed.main)?;

        // Per-entry sections: name -> Attributes in a real LinkedHashMap.
        let entries_map = p59_manifest_new_entries_map(ctx)?;
        let entries_pin = ctx.pin_native_root(entries_map);
        for (name, pairs) in &parsed.entries {
            let entry_attrs = p59_manifest_new_attributes(ctx)?;
            let entry_pin = ctx.pin_native_root(entry_attrs);
            p59_attrs_populate_real(ctx, entry_pin, entry_attrs, pairs)?;
            let ns = ctx.create_string(name);
            let ns_pin = ctx.pin_native_root(ns);
            let entries_map = ctx.read_native_pin(entries_pin, entries_map);
            let entry_attrs = ctx.read_native_pin(entry_pin, entry_attrs);
            let ns = ctx.read_native_pin(ns_pin, ns);
            ctx.invoke(
                "java/util/LinkedHashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[
                    Value::Object(Some(entries_map)),
                    Value::Object(Some(ns)),
                    Value::Object(Some(entry_attrs)),
                ],
            )?;
        }

        // Install on the Manifest, re-reading every ref after the allocations.
        let this = ctx.read_native_pin(this_pin, this);
        let main_attrs = ctx.read_native_pin(main_pin, main_attrs);
        let entries_map = ctx.read_native_pin(entries_pin, entries_map);
        ctx.set_field(this, 0, Value::Object(Some(main_attrs)));
        ctx.set_field(this, 1, Value::Object(Some(entries_map)));
        ctx.set_field_by_name(this, "attr", Value::Object(Some(main_attrs)));
        ctx.set_field_by_name(this, "entries", Value::Object(Some(entries_map)));
        Ok(None)
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// GC-safely populate a real `java.util.jar.Attributes` (pinned at `attrs_pin`)
/// from `pairs` via its genuine `putValue(String,String)` bytecode. Each
/// `create_string` / `putValue` can allocate and move objects, so the
/// Attributes is read back from its pin and the key string is pinned across the
/// value allocation. The key pins accumulate on the caller's pin batch and are
/// released when the caller unpins.
pub(crate) fn p59_attrs_populate_real(
    ctx: &mut dyn NativeContext,
    attrs_pin: usize,
    attrs_fallback: ObjectRef,
    pairs: &[(String, String)],
) -> Result<(), MethodCallFailed> {
    for (k, v) in pairs {
        let ks = ctx.create_string(k);
        let ks_pin = ctx.pin_native_root(ks);
        let vs = ctx.create_string(v);
        let attrs = ctx.read_native_pin(attrs_pin, attrs_fallback);
        let ks = ctx.read_native_pin(ks_pin, ks);
        ctx.invoke(
            "java/util/jar/Attributes",
            "putValue",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            &[
                Value::Object(Some(attrs)),
                Value::Object(Some(ks)),
                Value::Object(Some(vs)),
            ],
        )?;
    }
    Ok(())
}

// =============================================================================
// T10: Manifest(InputStream) synthetic constructor tests
//
// Covers the fix for the `JarFile.getManifest()` regression where
// native-io/src/zip_real_jar.rs called Manifest.<init>(InputStream) but
// only Manifest.<init>()V was registered in synthetic-jdk mode. These
// tests pin the parser's handling of CRLF, continuation lines, blank
// separators, and empty streams so the bug cannot silently reappear.
// =============================================================================

#[cfg(test)]
pub(crate) mod t10_manifest_input_stream_tests {
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
}

// ---------------------------------------------------------------------
// Round-12: zip 0.6 -> 2.x migration coverage. These tests pin the
// `SimpleFileOptions` API contract that every `start_file` call site in
// this crate now depends on. They live here because `phases_late.rs`
// hosts the only non-test zip-write production path (`zo_finalize_*` /
// ZipOutputStream synthetic methods) and the same API is shared with
// the four test-only write sites in this crate.
// ---------------------------------------------------------------------
#[cfg(test)]
pub(crate) mod zip_2x_api_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::io::{Cursor, Read, Write};

    /// Round-trip a two-entry ZIP through the 2.x writer + reader so a
    /// future accidental revert to 0.6 (or an API drift in 2.x) trips a
    /// dedicated, scoped test rather than only being caught by one of
    /// the surrounding integration tests.
    #[test]
    fn round_trip_simple_zip_two_entries() {
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.start_file("a.txt", opts).expect("start a.txt");
            zw.write_all(b"alpha").unwrap();
            zw.start_file("dir/b.bin", opts).expect("start dir/b.bin");
            zw.write_all(&[0u8, 1, 2, 3, 4, 5]).unwrap();
            zw.finish().expect("finish zip");
        }

        let inner = buf.into_inner();
        let mut archive =
            zip::ZipArchive::new(Cursor::new(inner)).expect("re-open round-tripped zip");
        assert_eq!(archive.len(), 2, "expected two entries");

        let mut s = String::new();
        archive
            .by_name("a.txt")
            .expect("a.txt entry")
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "alpha");

        let mut bin = Vec::new();
        archive
            .by_name("dir/b.bin")
            .expect("b.bin entry")
            .read_to_end(&mut bin)
            .unwrap();
        assert_eq!(bin, vec![0u8, 1, 2, 3, 4, 5]);
    }

    /// `SimpleFileOptions::default()` must surface a ZIP that the
    /// reader resolves as the conventional "deflate" method when an
    /// entry is added with the explicit `.compression_method(Deflated)`
    /// builder call — and `Stored` when configured for no compression.
    /// Callers across the crate rely on these two methods round-tripping
    /// to method codes 8 / 0 respectively (the SecurityManager
    /// signed-JAR fixtures store entries to keep digests byte-stable;
    /// the ZipOutputStream production path deflates).
    #[test]
    fn simple_file_options_compression_methods() {
        for (label, method, expected_code) in &[
            ("stored", zip::CompressionMethod::Stored, 0u16),
            ("deflated", zip::CompressionMethod::Deflated, 8u16),
        ] {
            let mut buf = Cursor::new(Vec::<u8>::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts = zip::write::SimpleFileOptions::default().compression_method(*method);
                zw.start_file(format!("{label}.dat"), opts).unwrap();
                zw.write_all(label.as_bytes()).unwrap();
                zw.finish().unwrap();
            }
            let bytes = buf.into_inner();
            let mut archive =
                zip::ZipArchive::new(Cursor::new(bytes)).expect("re-open per-method zip");
            let entry = archive
                .by_name(&format!("{label}.dat"))
                .expect("named entry present");
            // `.compression()` returns the configured `CompressionMethod`
            // — compare via the standard method code so this stays a
            // black-box assertion independent of 2.x enum layout.
            let got: u16 = match entry.compression() {
                zip::CompressionMethod::Stored => 0,
                zip::CompressionMethod::Deflated => 8,
                other => panic!("unexpected method {other:?} for {label}"),
            };
            assert_eq!(got, *expected_code, "method code mismatch for {label}");
        }
    }
}

#[cfg(test)]
pub(crate) mod bc_small_factors_tests {
    use super::{bc_has_any_small_factors, BC_SMALL_FACTOR_GROUPS};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn mag_le(mut v: u128) -> Vec<u32> {
        let mut w = Vec::new();
        while v != 0 {
            w.push((v & 0xFFFF_FFFF) as u32);
            v >>= 32;
        }
        w
    }

    #[test]
    fn group_products_fit_signed_int() {
        // Each group's modulus must equal the Java `int m` (positive, no overflow).
        for g in BC_SMALL_FACTOR_GROUPS {
            let m: u64 = g.iter().map(|&p| p as u64).product();
            assert!(
                m < (1u64 << 31),
                "group product {m} must fit a positive i32"
            );
        }
    }

    #[test]
    fn detects_small_factors() {
        for &n in &[0u128, 4, 9, 15, 21, 211, 211 * 211, 2 * 1_000_003] {
            assert!(
                bc_has_any_small_factors(&mag_le(n), false),
                "n={n} should report a small factor"
            );
        }
    }

    #[test]
    fn passes_values_with_no_small_factor() {
        assert!(!bc_has_any_small_factors(&mag_le(223), false)); // smallest prime > 211
        assert!(!bc_has_any_small_factors(&mag_le(1_000_003), false)); // prime
        assert!(!bc_has_any_small_factors(&mag_le((1u128 << 61) - 1), false)); // M61, prime
    }

    #[test]
    fn empty_mag_is_zero_divisible() {
        assert!(bc_has_any_small_factors(&[], false)); // x == 0
    }

    #[test]
    fn negative_fold_matches_nonneg_mod() {
        // BigInteger.mod is non-negative; divisibility by p is sign-invariant.
        assert!(bc_has_any_small_factors(&mag_le(15), true)); // -15 → factors 3,5
        assert!(bc_has_any_small_factors(&mag_le(211), true)); // -211 → factor 211
        assert!(!bc_has_any_small_factors(&mag_le(223), true)); // -223 → prime
    }

    // ---- util.BigIntegers.hasAnySmallFactors (odd primes 3..=743) ----
    use super::{bc_util_has_any_small_factors, BC_ODD_SMALL_PRIMES};

    fn sieve_odd_primes(limit: u32) -> Vec<u32> {
        let n = limit as usize;
        let mut s = vec![true; n + 1];
        (2..=n).for_each(|i| {
            if s[i] {
                let mut j = i * i;
                while j <= n {
                    s[j] = false;
                    j += i;
                }
            }
        });
        (3..=n)
            .filter(|&i| s[i] && i % 2 == 1)
            .map(|i| i as u32)
            .collect()
    }

    #[test]
    fn odd_small_primes_set_is_exactly_3_to_743() {
        // The array must be the prime factors of SMALL_PRIMES_PRODUCT (odd
        // primes 3..=743; verified out-of-band that their product == the BC
        // hex literal). Lock it against an independent sieve.
        assert_eq!(BC_ODD_SMALL_PRIMES.to_vec(), sieve_odd_primes(743));
    }

    #[test]
    fn util_detects_small_factors() {
        assert!(bc_util_has_any_small_factors(&mag_le(0))); // even (zero)
        assert!(bc_util_has_any_small_factors(&mag_le(2))); // even
        assert!(bc_util_has_any_small_factors(&mag_le(100))); // even
        assert!(bc_util_has_any_small_factors(&mag_le(743))); // factor 743
        assert!(bc_util_has_any_small_factors(&mag_le(743 * 1009))); // factor 743
        assert!(bc_util_has_any_small_factors(&mag_le(3 * 999_983))); // factor 3
    }

    #[test]
    fn util_passes_values_with_no_small_factor() {
        assert!(!bc_util_has_any_small_factors(&mag_le(751))); // smallest prime > 743
        assert!(!bc_util_has_any_small_factors(&mag_le(999_983))); // prime
        assert!(!bc_util_has_any_small_factors(&mag_le((1u128 << 61) - 1))); // M61
    }
}

#[cfg(test)]
mod jar_mtime_memo_tests {
    use super::{jar_cache_revalidate, jar_contents_cached, jar_path_mtime};
    use std::fs::File;
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    /// Write a one-entry zip at `path` whose single entry is named `entry`,
    /// then stamp it with `mtime` so the test does not depend on the host
    /// filesystem's timestamp granularity.
    fn write_jar(path: &std::path::Path, entry: &str, mtime: SystemTime) {
        let file = File::create(path).expect("create jar");
        let mut zipw = zip::ZipWriter::new(file);
        zipw.start_file(entry, zip::write::SimpleFileOptions::default())
            .expect("start_file");
        zipw.write_all(b"x").expect("write entry");
        let file = zipw.finish().expect("finish zip");
        file.set_modified(mtime).expect("set mtime");
        file.sync_all().expect("sync");
    }

    /// The accessors read a MEMOISED mtime, and opening re-probes it.
    ///
    /// Both halves are load-bearing and neither is incidental:
    ///
    /// * without the memo, every `JarFile.getEntry`/`size`/`getJarEntry` call
    ///   pays a `std::fs::metadata` (a `CreateFileW` on Windows, ~45 us on the
    ///   host this was measured on) — see `jar_path_mtime`;
    /// * without the re-probe on open, a jar rewritten on disk and reopened
    ///   would be served from the stale parse, which is the exact bug the
    ///   `(path, mtime)` cache key was introduced to prevent.
    #[test]
    fn rewritten_jar_is_stale_until_reopened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jar = dir.path().join("memo.jar");
        let path = jar.to_string_lossy().into_owned();

        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        write_jar(&jar, "first.txt", t0);

        // First touch parses and memoises the mtime.
        let first = jar_contents_cached(&path).expect("first parse");
        assert!(first.by_name.contains_key("first.txt"));
        let memoised = jar_path_mtime(&path);

        // Rewrite with a DIFFERENT entry and a distinctly later mtime.
        let t1 = t0 + Duration::from_secs(3600);
        write_jar(&jar, "second.txt", t1);

        // Still the snapshot taken at open: the accessors do not re-stat.
        assert_eq!(
            jar_path_mtime(&path),
            memoised,
            "the accessor path must not re-stat"
        );
        let stale = jar_contents_cached(&path).expect("cached parse");
        assert!(
            stale.by_name.contains_key("first.txt"),
            "an already-open jar keeps its snapshot, as HotSpot's ZipFile.Source does"
        );

        // Opening re-probes, and the changed mtime produces a new cache key.
        jar_cache_revalidate(&path);
        assert_ne!(
            jar_path_mtime(&path),
            memoised,
            "revalidation must observe the new mtime"
        );
        let fresh = jar_contents_cached(&path).expect("reparse");
        assert!(
            fresh.by_name.contains_key("second.txt"),
            "a reopened jar must be reparsed, not served stale"
        );
        assert!(!fresh.by_name.contains_key("first.txt"));
    }

    /// The memo's `LockLevel::Scratch` claim is actually EVALUATED by these
    /// tests, and the miss path does not re-enter the lock it just took.
    ///
    /// Two things worth one test between them:
    ///
    /// * `enforcement_active()` is unconditionally true in DEBUG builds, which
    ///   is what makes every other test in this module a live check of the
    ///   ordering claim rather than a run with the checker asleep. Asserting it
    ///   there means a future change that makes debug enforcement conditional
    ///   turns this module from silently vacuous into loudly red. The
    ///   assertion is `cfg`-gated because in RELEASE enforcement is off unless
    ///   `CRATONVM_LOCK_ORDER_CHECK` is set, and `cargo test --release` runs
    ///   this test too — an ungated assert here fails on a correct tree, which
    ///   is how the first version of it was caught.
    /// * `jar_path_mtime` on a MISS takes the memo lock, drops it, and only
    ///   then calls `jar_path_mtime_probe`, which takes it again. The guard
    ///   used to live in an `if let` scrutinee, where it outlives the body —
    ///   and `parking_lot` is not reentrant, so getting that wrong is a hang,
    ///   not a failure. A hang is exactly what this asserts the absence of, and
    ///   it is profile-independent, so that half runs in both.
    #[test]
    fn memo_lock_is_order_checked_and_not_re_entered_on_a_miss() {
        #[cfg(debug_assertions)]
        assert!(
            cratonvm_types::lock_order::enforcement_active(),
            "debug builds must enforce lock order, or this module's ordering \
             claim is never checked by anything"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let jar = dir.path().join("reentry.jar");
        let path = jar.to_string_lossy().into_owned();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_100_000_000);
        write_jar(&jar, "only.txt", t0);

        // Miss: lock, release, probe, lock again.
        let first = jar_path_mtime(&path);
        // Hit: the copied-out fast path.
        assert_eq!(jar_path_mtime(&path), first);
        // Re-probe, then hit again — the whole cycle, still single-entry.
        jar_cache_revalidate(&path);
        assert_eq!(jar_path_mtime(&path), first);
    }

    /// A path that does not exist must not pin a 0 mtime for the life of the
    /// process: a jar written later has to be seen.
    #[test]
    fn missing_path_is_not_memoised() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jar = dir.path().join("later.jar");
        let path = jar.to_string_lossy().into_owned();

        assert_eq!(jar_path_mtime(&path), 0, "absent file reports 0");
        assert!(jar_contents_cached(&path).is_none());

        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        write_jar(&jar, "late.txt", t0);

        assert_ne!(
            jar_path_mtime(&path),
            0,
            "a failed stat must not be memoised, or the file could never appear"
        );
        let contents = jar_contents_cached(&path).expect("parse after creation");
        assert!(contents.by_name.contains_key("late.txt"));
    }
}
