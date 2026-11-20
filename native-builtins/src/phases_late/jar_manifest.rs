// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

/// Register one of Spring Boot 3's `org/springframework/boot/loader/launch/`
/// launcher natives below in `--compatible` only.
///
/// They replace launcher bytecode wholesale -- `JarFileArchive` /
/// `ExplodedArchive.getClassPathUrls`, both `Archive.create` overloads and the
/// three `createClassLoader(Collection)` leaves -- for reasons that were all
/// about what the VM under the launcher could not do: no working `Stream`
/// pipeline over a native `JarFile`, a `File.isDirectory()` lost through the
/// `ProtectionDomain` URI round-trip, a typed `toArray` that threw. Under
/// `--real-jdk` that VM is the real JDK: `JarFile` is retired whole
/// (`RETIRED_SHADOW_JARFILE_TRIPLES`) and the stream, file and collection
/// families beneath it are real. What remained was the replacement, and it was
/// wrong: `JarFileArchive.getClassPathUrls` hands back an `ArrayList` where
/// the declared return is a `Set`, and it knows nothing of the `UNPACK:` entry
/// comment. `JarFileArchiveTests` failed 3 of 10 on exactly that
/// (`IncompatibleClassChangeError: Class java.util.ArrayList does not
/// implement the requested interface java.util.Set`) while HotSpot passed 10.
/// `--compatible` keeps every one unchanged.
#[track_caller]
fn register_sb3_launcher_compat_only(
    r: &mut NativeMethodRegistry,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: cratonvm_native_api::NativeCallback,
) {
    // `--real-jdk`, or `--compatible` with the `JdkJarFileBodies` row
    // (interpreter round i2 wave 58, lane L5): the `JarFile` is real there.
    if r.jdk_jar_file_bodies() {
        return;
    }
    r.register(class_name, method_name, descriptor, callback, cratonvm_native_api::NativeKind::SyntheticStub);
}

/// The synthetic `ExplodedArchive` both `Archive.create` overrides answer for
/// a directory: slot 0 the `File`, slot 1 the `/`-separated root path.
///
/// gc-common w19-g: shared by the two overrides, which each built it inline
/// with `file` held raw across the archive's allocation and the archive held
/// raw across the path string's. Both are rooted here and re-read before
/// every store.
fn p59_exploded_archive(
    ctx: &mut dyn NativeContext,
    file: ObjectRef,
    path: &str,
) -> MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let file_h = scope.root(file);
    let archive_new = try_alloc_concurrent_synthetic(
        &mut *scope,
        "org/springframework/boot/loader/launch/ExplodedArchive",
        3,
    )?;
    let archive_h = scope.root(archive_new);
    let root_uri_path = scope.create_string(&path.replace('\\', "/"));
    let archive = scope.get(&archive_h);
    let file_now = scope.get(&file_h);
    scope.set_field(archive, 0, Value::Object(Some(file_now)));
    scope.set_field(archive, 1, Value::Object(Some(root_uri_path)));
    Ok(Some(Value::Object(Some(archive))))
}

pub fn register_p59_jar(r: &mut NativeMethodRegistry) {
            let jf = "java/util/jar/JarFile";
    r.register(jf, "<init>", "(Ljava/lang/String;)V", p59_jar_file_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(jf, "<init>", "(Ljava/lang/String;Z)V", p59_jar_file_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(jf, "<init>", "(Ljava/io/File;)V", p59_jar_file_init_file, cratonvm_native_api::NativeKind::Bridge);
    // File-first overloads. `p59_jar_file_init_file` only reads args[1] (the
    // File) and ignores the rest, so the verify/mode/Runtime.Version variants
    // route through it unchanged. The 4-arg multi-release constructor
    // `JarFile(File, boolean, int, Runtime.Version)` is the one Tomcat's
    // AbstractArchiveResourceSet.openJarFile uses; without this it fell through
    // to real ZipFile bytecode and failed with "ZipException: zip file is empty"
    // (whole catalina.webresources JAR cluster).
    r.register(jf, "<init>", "(Ljava/io/File;Z)V", p59_jar_file_init_file, cratonvm_native_api::NativeKind::Bridge);
    r.register(jf, "<init>", "(Ljava/io/File;ZI)V", p59_jar_file_init_file, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        jf,
        "<init>",
        "(Ljava/io/File;ZILjava/lang/Runtime$Version;)V",
        p59_jar_file_init_file, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        jf,
        "getManifest",
        "()Ljava/util/jar/Manifest;",
        p59_jar_file_manifest, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        jf,
        "getManifestFromReference",
        "()Ljava/util/jar/Manifest;",
        p59_jar_file_manifest, cratonvm_native_api::NativeKind::Bridge,
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
                    // See `p59_attach_entry_attributes`: HotSpot's
                    // `JarFileEntry.getAttributes()` reads the manifest
                    // section for this name, and this VM answered null for
                    // every entry of every jar until it did too.
                    let ze = p59_attach_entry_attributes(ctx, ze, &path, &entry_name)?;
                    return Ok(Some(Value::Object(Some(ze))));
                }
            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
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
                        _ => return Ok(Some(Value::Object(None))),                    },                },
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
            p59_jar_lookup_versioned_entry(ctx, this, &path, &entry_name)        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(jf, "close", "()V", |ctx, args| {
        // JarFile is 2-field (path=0, manifest=1). Mark closed by clearing the path field.
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, Value::Object(None));
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // `java/util/zip/ZipFile.close()V` — JarFile does not declare its own
    // `close`, so a JarFile SUBCLASS's `super.close()` (an `invokespecial`,
    // e.g. Spring Boot's `NestedJarFile.close()`, `NestedJarFile.java:392`)
    // is resolved by JVMS method resolution past JarFile straight to
    // `ZipFile.close`. The `close` registration two lines up is keyed on
    // `java/util/jar/JarFile` and is never reached for that call -- so real
    // `ZipFile.close()` bytecode runs instead, reading the real `res` field
    // (a `ZipFile$CleanableResource`) `ZipFile.<init>`'s own registered
    // native never sets (this bridge stores its state in the compact
    // synthetic layout above and reads ZIP data with Rust's `zip` crate, by
    // design -- see `register_p59_jar`'s module doc). `res` stays permanently
    // null, and `ZipFile.close()` NPEs on `res.clean()`
    // (`zipfile-cleanableresource-npe-on-nestedjarfile-close-FIXED-20260924.md`).
    //
    // A plain, non-JarFile `ZipFile` never reaches this Bridge in a way that
    // matters: it has no JarFile-bridge-shaped construction to be
    // inconsistent with, so real bytecode is exactly right for it. Route on
    // the receiver's own lineage rather than force this globally.
    //
    // `Intrinsic`, not `Bridge`: `("java/util/zip/ZipFile", "close", "()V")`
    // is on `RETIRED_SHADOW_L1_TRIPLES` (a MEASURED "real bytecode is correct
    // here" verdict, for the overwhelming common case of a genuine, non-JarFile
    // `ZipFile`). `NativeMethodRegistry::register` auto-retags ANY `Bridge`
    // registered for a retired-shadow triple to `SyntheticStub`, which strict
    // `--real-jdk` then flatly rejects -- so a `Bridge` here can never win,
    // confirmed empirically (this exact closure, registered as `Bridge` first,
    // measurably changed nothing: `NestedJarFile.close()` kept reaching real
    // `ZipFile.close()` bytecode). `Intrinsic` is exempt from that retag and
    // is reviewed-and-semantics-preserving by definition -- which this is: for
    // every receiver the retirement's own measurement covers (non-JarFile),
    // this callback's `else` arm runs the IDENTICAL real bytecode
    // (`invoke_virtual_bytecode_only`), so the retirement's verdict still
    // holds; only the JarFile-lineage case, which that measurement never
    // exercised (real bytecode there is not "correct", it NPEs), is handled
    // differently.
    //
    // `--compatible` only (2026-09-24). Under `--real-jdk` `java/util/jar/
    // JarFile` is retired whole (`RETIRED_SHADOW_JARFILE_TRIPLES`), so no
    // native `JarFile` exists there: every one, `NestedJarFile`'s included, is
    // built by the real constructor and carries a real `res`. The JarFile arm
    // below would then be WRONG rather than merely unused -- it would skip the
    // real close of a real archive -- and the `else` arm is the bytecode the
    // retired row already runs. So the strict registry does not carry it, and
    // `no_retired_triple_survives_the_strict_boot` stops listing it as INERT.
    // The same shape as `nio_file.rs::register_vfs_windows_path_intrinsic`.
    // The `JdkJarFileBodies` row (wave 58, lane L5) is `--real-jdk`'s case
    // under `--compatible`: every `JarFile` is real (`jdk_jar_file_bodies`).
    if !r.jdk_jar_file_bodies() {
                        r.register("java/util/zip/ZipFile", "close", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if crate::classloader::object_extends(ctx, this, "java/util/jar/JarFile") {
                if ctx.object_num_fields(this) > 0 {
                    ctx.set_field(this, 0, Value::Object(None));
                }
                return Ok(None);
            }
            // A plain `ZipFile` that `native-io`'s constructor bridge opened
            // (`zip_real_jar::register_jar_natives` registers `<init>` on
            // `ZipFile` too) has no `res` either: the real body NPEd on
            // `res.clean()` for every `try (ZipFile z = new ZipFile(f))`
            // under `--compatible` (interpreter round i1 wave 49, lane L6;
            // `L6W49UrlAppHandlerOpenStream`, whose handler reads through
            // one). Release its handle the way that bridge's `close` does.
            if matches!(ctx.get_field_by_name(this, "res"), Value::Object(None)) {
                return cratonvm_native_io::zip_real_jar::native_jarfile_close(ctx, args);
            }
            Ok(ctx.invoke_virtual_bytecode_only(this, "close", "()V", &[])?)
        }, cratonvm_native_api::NativeKind::Intrinsic);
            }
    r.register(jf, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = ctx.get_field(this, 0);
        // HotSpot answers `file.getPath()`, the platform spelling: on Windows
        // the constructors above store the VM's `/`-separated path, and
        // `ModulePath.deriveModuleDescriptor` cuts an automatic module's name
        // at `lastIndexOf(File.separator)`, so every automatic module of a
        // `ModuleFinder.of(dir)` was named after its whole path
        // (`C.Users....auto.l6w49`; interpreter round i1 wave 49, lane L6,
        // `L6W49ModuleFinderOfJarDirectory`).
        #[cfg(windows)]
        if let Value::Object(Some(s)) = name {
            if let Some(path) = ctx.read_string(s).filter(|p| p.contains('/')) {
                let native = ctx.create_string_uninterned(&path.replace('/', "\\"));
                return Ok(Some(Value::Object(Some(native))));
            }
        }
        Ok(Some(name))
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
    );
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
    }, cratonvm_native_api::NativeKind::Bridge);
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
        p59_jar_file_stream, cratonvm_native_api::NativeKind::Bridge,
    );
    // JarFile.entries() — Enumeration<JarEntry>. Same backing data as
    // stream(); used by classpath scanners that prefer the legacy iteration.
    r.register(
        jf,
        "entries",
        "()Ljava/util/Enumeration;",
        p59_jar_file_entries, cratonvm_native_api::NativeKind::Bridge,
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
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(je, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `name` is slot 0 in both layouts, but read by name on the real layout
        // for symmetry with the other accessors.
        if je_is_real_layout(ctx, this)? {
            Ok(Some(ctx.get_field_by_name(this, "name")))
        } else {
            Ok(Some(ctx.get_field(this, 0)))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(je, "getComment", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if je_is_real_layout(ctx, this)? {
            Ok(Some(ctx.get_field_by_name(this, "comment")))
        } else {
            Ok(Some(ctx.get_field(this, 4)))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);

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
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        mf,
        "<init>",
        "(Ljava/io/InputStream;)V",
        p59_manifest_init_from_input_stream, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        mf,
        "<init>",
        "(Ljava/io/InputStream;Ljava/lang/String;)V",
        p59_manifest_init_from_input_stream, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        mf,
        "<init>",
        "(Ljava/util/jar/JarVerifier;Ljava/io/InputStream;Ljava/lang/String;)V",
        p59_manifest_init_from_verified_input_stream, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        mf,
        "<init>",
        "(Ljava/util/jar/Manifest;)V",
        p59_manifest_init_copy, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(mf, "getEntries", "()Ljava/util/Map;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let by_name = ctx.get_field_by_name(this, "entries");
        if matches!(by_name, Value::Object(Some(_))) {
            return Ok(Some(by_name));
        }
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // NOTE: java.util.jar.Attributes is backed by a single real `map`
    // (LinkedHashMap) field — see p59_manifest_new_attributes. getValue /
    // putValue / size / entrySet therefore run the genuine JDK bytecode over
    // that map; we deliberately do NOT register synthetic overrides here (the
    // old slot-walking overrides were wrong for the 1-field real layout and
    // returned null/0 once the gen_heap OOB guard started dropping their
    // out-of-bounds slot accesses).

    // The `Attributes.Name` constants `MANIFEST_VERSION` / `MAIN_CLASS` were
    // registered here with the FIELD descriptor `Ljava/util/jar/Attributes$Name;`
    // and minted a 1-slot `Name` without `<init>`. Gone (round 14 wave 2, lane
    // shadow; SH4-2's residue): no door can dispatch a field-shaped key and
    // `getstatic` has no registry path
    // (`docs/feature-designs/native-dispatch-architecture.md`); the
    // real `<clinit>` publishes both.

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
    register_sb3_launcher_compat_only(
        r,
        jfa,
        "getClassPathUrls",
        "(Ljava/util/function/Predicate;Ljava/util/function/Predicate;)Ljava/util/Set;",
        p59_spring_boot_jar_archive_get_class_path_urls,
    );
    register_sb3_launcher_compat_only(
        r,
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
    register_sb3_launcher_compat_only(
        r,
        "org/springframework/boot/loader/launch/Archive",
        "create",
        "(Ljava/io/File;)Lorg/springframework/boot/loader/launch/Archive;",
        |ctx, args| {
            let file_arg = obj_arg(args, 0)?;
            // gc-common w19-g: `file` is rooted across `getPath()` (Java) and
            // the archive's allocation, and the archive across its root-path
            // string; all were held raw. See `p59_exploded_archive`.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let file_h = scope.root(file_arg);
            let path = file_read_path(&mut *scope, file_arg);
            let file = scope.get(&file_h);
            if std::path::Path::new(&path).is_dir() {
                return p59_exploded_archive(&mut *scope, file, &path);
            }
            scope.new_object_initialized(
                "org/springframework/boot/loader/launch/JarFileArchive",
                "(Ljava/io/File;)V",
                &[Value::Object(Some(file))],
            )
        },
    );
    register_sb3_launcher_compat_only(
        r,
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
                p59_exploded_archive(ctx, file, &path)
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
        register_sb3_launcher_compat_only(
            r,
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
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        launcher_base,
        "createClassLoader",
        "(Ljava/util/Iterator;)Ljava/lang/ClassLoader;",
        sb2_launcher_create_class_loader_bypass_archive_walk, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        launcher_base,
        "createClassLoader",
        "(Ljava/util/List;)Ljava/lang/ClassLoader;",
        sb2_launcher_create_class_loader_bypass_archive_walk, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getMainClass",
            "()Ljava/lang/String;",
            sb2_launcher_get_main_class, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(cls, "isExploded", "()Z", sb2_launcher_is_exploded, cratonvm_native_api::NativeKind::Bridge);
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
            |_ctx, _args| Ok(Some(Value::Int(0))), cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getClassPathArchives",
            "()Ljava/util/List;",
            sb2_launcher_get_class_path_archives_list, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getClassPathArchivesIterator",
            "()Ljava/util/Iterator;",
            sb2_launcher_get_class_path_archives_iterator, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "createClassLoader",
            "(Ljava/util/Iterator;)Ljava/lang/ClassLoader;",
            sb2_launcher_create_class_loader_bypass_archive_walk, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "createClassLoader",
            "(Ljava/util/List;)Ljava/lang/ClassLoader;",
            sb2_launcher_create_class_loader_bypass_archive_walk, cratonvm_native_api::NativeKind::Bridge,
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
            |_ctx, _args| Ok(Some(Value::Object(None))), cratonvm_native_api::NativeKind::Bridge,
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
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
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
        crate::classloader::ucl_find_resource, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
        spring_class_utils_for_name_impl, cratonvm_native_api::NativeKind::Bridge,
    );
    // Some Spring Boot 3 / Spring Framework 6 code paths use a 1-arg overload.
    r.register(
        cu,
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        spring_class_utils_for_name_impl, cratonvm_native_api::NativeKind::Bridge,
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
    //     spring_default_app_ctx_factory_create, cratonvm_native_api::NativeKind::Bridge);
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
pub fn spring_class_utils_for_name_impl(
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
        // `is_user_defined_loader` answers `false` for a BARE
        // `java.net.URLClassLoader`/`ClassLoader` instance by design
        // (`is_builtin_loader_class` lists them -- see that predicate's own doc
        // comment): it is aimed at "does this receiver need per-instance
        // ClassLoader handling", not at the question asked HERE -- "should
        // this explicit `classLoader` argument's own resolution win over the
        // global scanner". A bare `URLClassLoader` parented to some OTHER
        // user-defined loader (not the process-wide app/platform singleton --
        // exactly the shape `new URLClassLoader(urls, getClass().
        // getClassLoader())` constructs when the enclosing class was itself
        // loaded by e.g. Spring Boot's `ModifiedClassPathClassLoader`, as
        // `MockWebEnvironmentServletComponentScanIntegrationTests.
        // indexedComponentsAreRegistered` does) is neither `is_user_defined_loader`
        // NOR caught by `url_classloader_isolated_from_app` (that predicate
        // only looks ONE hop up the parent chain for platform/null -- a
        // loader parented to another user loader is correctly "not isolated"
        // by ITS definition, a different question again), so it fell through
        // to the loader-BLIND global scanner below. When 2+ sibling
        // `ModifiedClassPathClassLoader` forks (one per `@ForkedClassPath`
        // test method, all in the same process -- see that class's own doc
        // comment, there is no real subprocess fork) had each already defined
        // their OWN copy of the requested name, the global scanner's own
        // ambiguity guard (`resolve_fast_path_class_id`'s "more than one user
        // loader owns this name" check) returned no answer, and the fallback
        // minted a THIRD, `Application`-scoped copy -- distinct from the
        // fork's own copy every other resolution in that fork already uses,
        // surfacing as `ConversionNotSupportedException: Cannot convert value
        // of type 'jakarta.servlet.DispatcherType' to required type
        // 'jakarta.servlet.DispatcherType'` (two same-named, unequal `Class`
        // objects). The actually-correct question -- "is `loader` neither
        // null nor one of the two built-in process-wide singletons" -- is
        // exactly what `is_application_class_loader`/`is_platform_class_loader`
        // answer; a bare instance parented to a THIRD-PARTY user loader is
        // correctly included by excluding only those two, at any chain depth.
        let is_boot = ctx
            .class_name_of_id(ctx.class_id_of_object(loader))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$BootClassLoader");
        if !crate::classloader::is_application_class_loader(ctx, loader)
            && !crate::classloader::is_platform_class_loader(ctx, loader)
            && !is_boot
        {
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
    // classutils-forname-platform-loader-false-positive.md).
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
pub fn spring_default_app_ctx_factory_create(
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
pub struct JarEntryRec {
    pub size: i64,
    pub csize: i64,
    pub method: i32,
    pub crc: i64,
    pub comment: Option<String>,
    pub times: JarEntryTimes,
}

pub use cratonvm_native_io::zip_real_jar::ZipEntryTimes as JarEntryTimes;

/// Whole-jar parsed contents: per-name records plus central-directory order.
pub struct JarContents {
    pub by_name: std::collections::HashMap<String, JarEntryRec>,
    pub order: Vec<String>,
    /// Memoized `Multi-Release: true` answer (JEP 238) for this jar.
    ///
    /// `p59_jar_lookup_versioned_entry` asks once per ENTRY lookup, and
    /// answering costs a whole-manifest `from_utf8_lossy` plus a line-by-line
    /// parse. On a Tomcat webapp deploy - hundreds of jars, thousands of entry
    /// lookups - that re-parse was 8.3% of the entire run. Living on
    /// `JarContents` keys it on the same (path, mtime) pair as the rest of the
    /// jar cache, so it invalidates exactly when the jar does.
    pub multi_release: std::sync::OnceLock<bool>,
}

pub fn p59_set_jar_entry_times(
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
pub fn jar_cache_revalidate(path: &str) {
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
pub fn jar_contents_cached(path: &str) -> Option<std::sync::Arc<JarContents>> {
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
    let mut raw = file.try_clone().ok()?;
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
        let name = entry.name().ok()?.into_owned();
        let size = entry.size() as i64;
        let csize = entry.compressed_size() as i64;
        #[allow(deprecated)]
        let method = entry.compression().to_u16() as i32;
        let crc = entry.crc32() as i64 & 0xFFFF_FFFFi64;
        let comment = (!entry.comment().is_empty()).then(|| entry.comment().to_owned());
        let (_, times) =
            cratonvm_native_io::zip_real_jar::zip_entry_extra_metadata(&entry, &mut raw).ok()?;
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
/// Cap on how many distinct jars' `ARCHIVES` entries (each a live
/// `zip::ZipArchive<File>`, i.e. one held-open OS file descriptor) this
/// process keeps around at once. `ARCHIVES` used to grow without bound —
/// fine for a normal single-application JVM, which touches a small,
/// bounded set of jars, but a batch test harness that runs many classes
/// per process (`CratonRunner <class>...`) can call `getInputStream`/
/// `getEntry` against thousands of DISTINCT jars over the process's life
/// (every `ServiceLoader.load()` reads `META-INF/services/*` out of every
/// jar on the classpath), so the never-evicted cache eventually exhausted
/// the process's file descriptor table (`EMFILE`, "Too many open files")
/// after roughly `ulimit -n` distinct jars were ever touched — surfacing
/// as unrelated-looking crashes (`ServiceConfigurationError`,
/// `FileNotFoundException`) partway through a multi-class batch run, not
/// as an OOM or an obvious leak. Bounding this cache and evicting the
/// oldest entry once it is full trades a re-parse of that jar's central
/// directory on its next touch for never exhausting the descriptor table.
/// Overridable via `CRATONVM_JAR_ARCHIVE_CACHE_CAP` for hosts with a much
/// higher or lower `ulimit -n`.
fn max_open_jar_archives() -> usize {
    use std::sync::OnceLock;
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JAR_ARCHIVE_CACHE_CAP")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(256)
    })
}

pub fn jar_entry_bytes_cached(
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
    // Bounded (see `max_open_jar_archives`) so this cache cannot exhaust the
    // process's file descriptor table on a workload that touches many
    // distinct jars; `ARCHIVE_ORDER` records insertion order for FIFO
    // eviction once it fills up.
    static ARCHIVES: OnceLock<
        Mutex<std::collections::HashMap<String, Arc<Mutex<zip::ZipArchive<std::fs::File>>>>>,
    > = OnceLock::new();
    // `OrderedPlMutex`, not a raw `Mutex`, and the distinction is load-bearing
    // in THIS crate specifically. `native-builtins` re-enters the VM — a
    // native callback calls back into Java, which takes the heap and
    // class-manager locks — so a global lock here with no `LockLevel` is a
    // deadlock the order checker cannot see. `lock_discipline_ratchet` counts
    // raw constructions for exactly that reason and says not to raise its
    // baseline.
    //
    // `Scratch` is the leaf level, which is what this deque is: it holds
    // nothing else while locked, calls nothing, and is released before the
    // archive it names is touched. `mtime_memo` and the section `CACHE` in
    // this same file are already `Scratch` for the same reason, and none of
    // the three is ever held across another, so the "equal or lower level
    // panics" rule is not in play.
    static ARCHIVE_ORDER: OnceLock<OrderedPlMutex<std::collections::VecDeque<String>>> =
        OnceLock::new();
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
        let result = archives
            .entry(archive_key.clone())
            .or_insert_with(|| opened.clone())
            .clone();
        // Only the thread that actually won the insert (not a racer that hit
        // the `or_insert_with` closure's already-present branch) records the
        // key and drives eviction, so a concurrent open of the same jar never
        // double-counts it in `ARCHIVE_ORDER`.
        if Arc::ptr_eq(&result, &opened) {
            let order = ARCHIVE_ORDER.get_or_init(|| {
                OrderedPlMutex::new(std::collections::VecDeque::new(), LockLevel::Scratch)
            });
            // `OrderedPlMutex::lock` returns the guard directly: a
            // `parking_lot` mutex has no poisoning, so there is no
            // `unwrap_or_else(into_inner)` to write here.
            let mut order = order.lock();
            order.push_back(archive_key);
            let cap = max_open_jar_archives();
            while archives.len() > cap {
                let Some(oldest) = order.pop_front() else {
                    break;
                };
                archives.remove(&oldest);
            }
        }
        result
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

/// The manifest's PER-ENTRY sections, parsed once per (path, mtime).
///
/// `Manifest.getEntries()` is already answered from a full parse elsewhere;
/// this cache exists because `JarEntry.attr` has to be decided at every entry
/// materialization, and re-parsing `META-INF/MANIFEST.MF` per `getEntry` call
/// would put a parse on the path that `jar_contents_cached`'s doc comment
/// spent a whole paragraph taking a decompress off. The bytes themselves come
/// from `jar_entry_bytes_cached`, so a jar with no manifest costs one miss.
///
/// An absent or unparseable manifest caches the EMPTY map rather than
/// returning `None` on every call: the answer "this jar has no sections" is as
/// cacheable as any other, and a negative that is not cached is a re-read per
/// entry on exactly the jars that have nothing to read.
pub fn jar_manifest_sections_cached(
    path: &str,
) -> Option<std::sync::Arc<std::collections::HashMap<String, Vec<(String, String)>>>> {
    use std::sync::{Arc, OnceLock};
    type Sections = std::collections::HashMap<String, Vec<(String, String)>>;
    // LEVEL (lock-discipline ratchet): `Scratch` is L0, the bottom of the
    // hierarchy -- a thread holding it may acquire NOTHING else, not even
    // another `Scratch`. This cache meets that the same way `mtime_memo` above
    // does, and the argument is the same one in the same order:
    //
    // * Its two critical sections are a `get(&str).cloned()` and an
    //   `insert(String, Arc<_>)` on a plain `HashMap` of owned values. Neither
    //   takes another lock.
    // * `jar_manifest_sections_cached` takes no `NativeContext`, so it *cannot*
    //   re-enter the VM while holding this -- which is the cycle this crate's
    //   locks actually risk, since a native callback calling back into Java
    //   takes the heap and the L10 class-manager lock.
    // * The two calls it makes that DO take locks are both outside its critical
    //   sections, and deliberately: `jar_path_mtime` (which takes the `Scratch`
    //   mtime memo) runs before the first `lock()`, and `jar_entry_bytes_cached`
    //   (which takes the two raw jar caches) runs between them. Two `Scratch`
    //   locks nested would violate this level as surely as a higher one would,
    //   so keep the mtime lookup where it is: it exists only to build the key.
    static CACHE: OnceLock<OrderedPlMutex<std::collections::HashMap<String, Arc<Sections>>>> =
        OnceLock::new();
    if path.is_empty() {
        return None;
    }
    let mtime = jar_path_mtime(path);
    let key = format!("{path}\u{0}{mtime}");
    let cache = CACHE
        .get_or_init(|| OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch));
    // Bound to a `let` rather than read in the `if let` scrutinee: a scrutinee's
    // temporary lives to the end of the whole `if let`, so the guard's release
    // point would depend on a rule about temporaries rather than on anything
    // visible here. The claim the level makes is worth making structurally.
    let hit = cache.lock().get(&key).cloned();
    if let Some(s) = hit {
        return Some(s);
    }
    let sections: Sections = jar_entry_bytes_cached(path, "META-INF/MANIFEST.MF")
        .and_then(|bytes| p59_parse_manifest_bytes(&bytes).ok())
        .map(|parsed| parsed.entries.into_iter().collect())
        .unwrap_or_default();
    let sections = Arc::new(sections);
    cache.lock().insert(key, sections.clone());
    Some(sections)
}

/// Give a freshly minted `JarEntry` the `attr` a real one would have, and hand
/// back the (possibly relocated) entry.
///
/// WHAT WAS WRONG. `java.util.jar.JarEntry.getAttributes()` is, in its
/// entirety, `return attr;` — verified with `javap -c` against the JDK 25
/// image this VM boots, not assumed. The JDK never leaves that field for the
/// caller to fill: `JarFile.getEntry` hands back a `JarFile$JarFileEntry`,
/// whose `getAttributes()` reads `jarfile.getManifest().getAttributes(name)`.
/// This VM mints a plain `JarEntry` and never wrote `attr`, so
/// `getAttributes()` answered null for every entry of every jar — including
/// the ones whose manifest has a section for exactly that entry. Measured by
/// `apps/probes/L1JarTextSweep`'s `E.attributesFromJar` row, which reads
/// `null` where HotSpot reads the section's `Section-Key`.
///
/// It is the JarFile family's defect and not `JarEntry`'s: `JarEntry` declares
/// no native here at all, so nothing wave 4 could have retired repairs it.
///
/// WHAT THIS IS NOT. It is not the `JarFile` state model that section 10 item
/// 5 prices — the entry is still a synthetic `JarEntry` rather than a
/// `JarFileEntry` bound to a live `ZipFile`. It fills one field that the real
/// `getAttributes()` bytecode then reads on its own, which is why the repair
/// needs no native on `JarEntry` and no new carrier class.
///
/// A jar whose manifest has no section for this entry leaves `attr` null,
/// which is what HotSpot answers there too. On the 4-slot synthetic layout the
/// named write finds no field and is a no-op, leaving the previous behaviour
/// exactly as it was.
pub fn p59_attach_entry_attributes(
    ctx: &mut dyn NativeContext,
    je: ObjectRef,
    path: &str,
    entry_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let sections = match jar_manifest_sections_cached(path) {
        Some(s) => s,
        None => return Ok(je),
    };
    let pairs = match sections.get(entry_name) {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return Ok(je),
    };
    // GC-SAFETY: everything below runs Java code — `Attributes.<init>` and one
    // `putValue` per pair — and both `je` and the fresh `Attributes` are bare
    // Rust locals the collector cannot see. Root both, re-read both, and
    // release only after the last allocating call.
    let je_pin = ctx.pin_native_root(je);
    let attrs = p59_manifest_new_attributes(ctx)?;
    let attrs_pin = ctx.pin_native_root(attrs);
    p59_attrs_populate_real(ctx, attrs_pin, attrs, &pairs)?;
    let attrs = ctx.read_native_pin(attrs_pin, attrs);
    let je = ctx.read_native_pin(je_pin, je);
    // Releases both pins: `unpin_native_roots` truncates to the index given,
    // and `je_pin` is the earlier of the two. Nothing below allocates.
    ctx.unpin_native_roots(je_pin);
    ctx.set_field_by_name(je, "attr", Value::Object(Some(attrs)));
    Ok(je)
}

#[cfg(test)]
mod w6_entry_attribute_tests;

/// Read the central directory of `path` and return a Vec of allocated
/// synthetic `java/util/jar/JarEntry` ObjectRefs. Returns an empty Vec on
/// any I/O / zip-parse error so callers see an empty Stream rather than
/// an exception path.
pub fn p59_jar_collect_entries(
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
        // Same reason as in `p59_jar_lookup_entry`: an entry that arrived
        // through `entries()`/`stream()` answers `getAttributes()` from the
        // manifest on HotSpot too, and a jar with no sections pays one cached
        // map lookup per entry for it.
        let je = p59_attach_entry_attributes(ctx, je, path, name)?;
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
            // Down to 8 (`JarFile.BASE_VERSION_FEATURE`), as JDK 25's
            // `getVersionedEntry` walks (wave 104, lane L5; it stopped at 9).
            (8..=runtime_feature).rev().find_map(|version| {
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
pub fn p59_jar_lookup_entry(
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
    // `JarFile.getEntry`/`getJarEntry` hand back a `JarFileEntry` on HotSpot,
    // whose `getAttributes()` reads the manifest section for this name.
    let je = p59_attach_entry_attributes(ctx, je, path, &name)?;
    Ok(Value::Object(Some(je)))
}

pub fn p59_jar_file_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub fn p59_fat_jar_boot_inf_nested_url_values(
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
                        let name = entry
                            .name()
                            .map_err(|e| RuntimeError::IOException {
                                message: e.to_string(),
                            })?
                            .into_owned();
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
pub fn p59_spring_boot_jar_archive_get_class_path_urls(
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
                _ => String::new(),            }        }
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
pub fn p59_spring_boot_exploded_archive_get_class_path_urls(
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
pub fn sb3_executable_archive_launcher_create_class_loader_collection(
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
pub fn sb2_launcher_jar_path(
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
pub fn sb2_launcher_is_exploded(
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
pub fn sb2_read_start_class(jar_path: &str) -> Option<String> {
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
pub fn sb2_launcher_get_main_class(
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
pub fn sb2_launcher_build_archive_list(
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
                    let name = entry
                        .name()
                        .map_err(|e| RuntimeError::IOException {
                            message: e.to_string(),
                        })?
                        .into_owned();
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
pub fn sb2_launcher_get_class_path_archives_list(
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
pub fn sb2_launcher_get_class_path_archives_iterator(
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
pub fn sb2_launcher_create_class_loader_bypass_archive_walk(
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
pub fn p59_alloc_url(
    ctx: &mut dyn NativeContext,
    full: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // SBR wave 2 (lane `netaddr`): `--real-jdk` runs the URL's own JDK bodies
    // (`toString` is `handler.toExternalForm(this)`), so the URL is built by
    // the JDK constructor and carries its handler. `--compatible` keeps the
    // slot-written carrier below.
    if let Some(url) = crate::jboss_module_loader::jdk_only_constructed_url(ctx, full) {
        return Ok(url);
    }
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
    crate::jboss_module_loader::url_write_declared_field_defaults(&*ctx, url);
    ctx.unpin_native_roots(url_pin);
    Ok(url)
}

pub fn p59_jar_file_entries(
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
pub fn p59_jar_file_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub fn p59_jar_file_init_file(
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

pub fn p59_jar_file_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => match ctx.get_field_by_name(this, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
    }
}

pub fn p59_jar_file_manifest(
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
pub fn p98_read_jar_manifest(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<Value, MethodCallFailed> {
    Ok(p98_read_jar_manifest_impl(ctx, path, true)?)
}

/// Constructor variant of [`p98_read_jar_manifest`]. The main attributes are
/// sufficient during JarFile construction; signed entry sections are expanded
/// only when a caller asks for the full manifest.
pub fn p98_read_jar_manifest_main(
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
pub fn p59_manifest_new_attributes(
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
pub fn p59_manifest_new_entries_map(
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
pub fn p59_read_input_stream_fully(
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
pub fn p59_parse_manifest_bytes(data: &[u8]) -> Result<ParsedManifest, String> {
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

pub struct ParsedManifest {
    main: Vec<(String, String)>,
    entries: Vec<(String, Vec<(String, String)>)>,
}

#[cfg(test)]
pub mod manifest_parser_tests;

/// Synthetic `java.util.jar.Manifest.<init>(InputStream)`. Reads the full
/// stream, parses it as a MANIFEST.MF, and installs the resulting
/// mainAttributes / entries onto `this`. On any IO or parse error throws
/// `java.io.IOException` rather than panicking.
pub fn p59_manifest_init_from_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    p59_manifest_init_from_input_stream_at(ctx, args, 1)
}

pub fn p59_manifest_init_from_verified_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    p59_manifest_init_from_input_stream_at(ctx, args, 2)
}

pub fn p59_manifest_init_copy(
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

pub fn p59_manifest_init_from_input_stream_at(
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
pub fn p59_attrs_populate_real(
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
pub mod t10_manifest_input_stream_tests;

// ---------------------------------------------------------------------
// Round-12: zip 0.6 -> 2.x migration coverage. These tests pin the
// `SimpleFileOptions` API contract that every `start_file` call site in
// this crate now depends on. They live here because `phases_late.rs`
// hosts the only non-test zip-write production path (`zo_finalize_*` /
// ZipOutputStream synthetic methods) and the same API is shared with
// the four test-only write sites in this crate.
// ---------------------------------------------------------------------
#[cfg(test)]
pub mod zip_2x_api_tests;

#[cfg(test)]
pub mod bc_small_factors_tests;

#[cfg(test)]
mod jar_mtime_memo_tests;
