#[allow(unused_imports)]
use super::super::*;

#[allow(unused_variables)]
pub fn register_essential_natives_with_shims_c(registry: &mut NativeMethodRegistry, shim_selection: app_shims::ShimSelection) {
    registry.register(
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        lang_class::native_annotated_type_get_annotation, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl",
        "getAnnotatedOwnerType",
        "()Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_annotated_type_get_annotated_owner_type, cratonvm_native_api::NativeKind::Bridge,
    );
    // `AnnotatedParameterizedType.getAnnotatedActualTypeArguments()` --
    // surfaces TYPE_ARGUMENT-level annotations (e.g. `@Valid` in
    // `List<@Valid Person>`) that real-JDK's null `getTypeAnnotationBytes0`
    // stub can never recover. See
    // `native_annotated_parameterized_type_get_annotated_actual_type_arguments`
    // doc comment for the full rationale (SC-web-method-validation RC-B).
    registry.register(
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedParameterizedTypeImpl",
        "getAnnotatedActualTypeArguments",
        "()[Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_annotated_parameterized_type_get_annotated_actual_type_arguments, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getClassFileVersion0",
        "()I",
        lang_class::native_class_get_class_file_version,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getClassAccessFlagsRaw0",
        "()I",
        lang_class::native_class_get_class_access_flags_raw,
        NativeKind::Bridge,
    );
    // T19.N1: Class security natives — expose the CodeSource installed at
    // class-define time via `Class.getProtectionDomain()` / `.getSigners()`
    // so reflective policy code can see per-class origin + signer info.
    registry.register(
        "java/lang/Class",
        "getProtectionDomain0",
        "()Ljava/security/ProtectionDomain;",
        lang_class::native_class_get_protection_domain0, cratonvm_native_api::NativeKind::Bridge,
    );
    // JDK 25 implements the public method as a direct read of Class's private
    // `protectionDomain` field. Our synthetic mirrors cannot rely on that
    // VM-populated field, so route the public surface through the same
    // class-id-backed CodeSource implementation as the legacy native hook.
    registry.register(
        "java/lang/Class",
        "getProtectionDomain",
        "()Ljava/security/ProtectionDomain;",
        lang_class::native_class_get_protection_domain, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getSigners",
        "()[Ljava/lang/Object;",
        lang_class::native_class_get_signers, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "setSigners",
        "([Ljava/lang/Object;)V",
        lang_class::native_class_set_signers, cratonvm_native_api::NativeKind::Bridge,
    );

    // C14: `Class.getEnumConstants()` and the package-private
    // `Class.getEnumConstantsShared()` both need to return the enum's
    // populated `$VALUES` array.  The real JDK implements these in Java
    // using reflection on `values()`, but our reflection stack's Method
    // layout / invoke path returned null, causing
    // `EnumMap.getKeyUniverse` → `arraylength null` → silent-swallow of
    // `StreamOpFlag.<clinit>`.  Registering a native override on both
    // entry points (callable in real-JDK mode as well as synthetic) is
    // the root-cause fix.
    registry.register(
        "java/lang/Class",
        "getEnumConstants",
        "()[Ljava/lang/Object;",
        lang_class::native_class_get_enum_constants,
        NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getEnumConstantsShared",
        "()[Ljava/lang/Object;",
        lang_class::native_class_get_enum_constants,
        NativeKind::Intrinsic,
    );

    // --- Class.getProtectionDomain() ---
    // T15: Return a synthetic ProtectionDomain with a populated CodeSource
    // whose URL.getPath() resolves to the classpath entry (JAR or directory)
    // that holds the class.  This satisfies Quarkus/Spring/etc. code paths
    // that derive their application root from `getProtectionDomain().getCodeSource().getLocation().getPath()`.
    //
    // Layouts, as the body BELOW actually writes them — this block described a
    // different object until 2026-08-13 and was contradicted 100 lines further
    // down inside its own closure:
    //   ProtectionDomain — slot 0: CodeSource; slot 1 is `classloader`, NOT
    //                      `Permissions` (see the SBR-13 note at the tail — the
    //                      Permissions-on-slot-1 layout is the bug that note
    //                      records fixing). Everything else goes in by name via
    //                      `populate_protection_domain_fields`.
    //   CodeSource       — written BY NAME (`location`, `certs`); slot 1 is
    //                      `signers` on the real class, so the raw-slot form
    //                      was wrong here too.
    //   URL              — the real JDK 25 13-field order, NOT a 3-field
    //                      synthetic one: 0=protocol, 1=host, 2=port(int),
    //                      3=file, 4=query, 5=authority, 6=path, 7=userInfo,
    //                      8=ref, 9=hostAddress, 10=handler, 11=hashCode(int),
    //                      12=tempState. Confirmed against `javap -p java.net.URL`
    //                      on JDK 25.0.3.
    //
    // SHADOWED: this registration has no runtime effect. `Class.getProtectionDomain()`
    // is registered THREE times in this one function — at `:13017` and again at
    // `:13234`, both with `lang_class::native_class_get_protection_domain0`, with
    // this closure sandwiched between them. `register()` is
    // last-registration-wins, so `:13234` answers and the three allocations in
    // this closure (`ProtectionDomain` 4, `URL` 13, `CodeSource` 2) never run.
    // Kept because
    // the two bodies are not identical and the live one has no equivalent of
    // this one's `jar:file:` handling; deleting it is a behaviour question, not
    // a cleanup. See
    // docs/feature-designs/native-dispatch-architecture.md §4.
    registry.register(
        "java/lang/Class",
        "getProtectionDomain",
        "()Ljava/security/ProtectionDomain;",
        |ctx, args| {
            let pd = try_alloc_concurrent_synthetic(ctx, "java/security/ProtectionDomain", 4)?;
            // GC-safety: `pd` is allocated FIRST and written LAST, and almost
            // everything between the two allocates — a `java.net.URL`, a
            // `CodeSource`, several strings, and a nested
            // `native_class_get_class_loader`. A moving young collection at any
            // of those relocates `pd`, and this closure then populates and
            // RETURNS the pre-move address.
            //
            // The interpreter's return-value barrier cannot save it:
            // `safe_native_call` heals a stale return through
            // `load_and_forward`, which reads the forwarding marker at the old
            // address — and that marker is gone once the allocator has
            // re-served the span, which is exactly the window this workload
            // reads it in. MEASURED on BindableTests under
            // `CRATONVM_DBG_GC_STRESS=262144`: `[deadref-nret]` named this
            // native returning an address in the emptied semispace, reached
            // from ByteBuddy's `trySelfResolve(Class)`.
            let pd_pin = ctx.pin_native_root(pd);
            // Try to produce a real CodeSource with a URL pointing at the
            // classpath entry that holds this Class.
            let mut path_opt = if let Some(Value::Object(Some(mirror))) = args.first() {
                // The code source belongs to a class *identity*, not merely a
                // binary name. `find_class_source_path(name)` is intentionally
                // loader-blind and reports the application copy when an
                // isolated URLClassLoader defines its own same-named class.
                // Prefer the CodeSource saved on this mirror's exact ClassId;
                // retain the old path lookup only for legacy mirrors that have
                // no CodeSource metadata.
                ctx.class_id_from_mirror(*mirror).and_then(|cid| {
                    ctx.class_code_base(cid).or_else(|| {
                        ctx.class_name_of_id(cid)
                            .and_then(|name| ctx.find_class_source_path(&name))
                    })
                })
            } else {
                None
            };
            // `find_class_source_path` may return `outer.jar!/nested` for JAR-in-JAR;
            // Spring Boot's `Archive.create(File)` needs the outer file only.
            if let Some(ref mut p) = path_opt {
                if let Some(idx) = p.find('!') {
                    p.truncate(idx);
                }
            }
            // JDK `Class` mirrors often lack a reverse-map entry early in boot;
            // Spring Boot 3.2's loader calls `Archive.create(Launcher.class)` and
            // needs a non-null `CodeSource`. Fall back to the first `java.class.path`
            // entry (the executable JAR for `cratonvm --jar …`).
            let path_opt = path_opt.or_else(|| {
                ctx.get_system_property("java.class.path").and_then(|cp| {
                    let sep = if cfg!(windows) { ';' } else { ':' };
                    cp.split(sep)
                        .next()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                })
            });
            let codesource = if let Some(raw_path) = path_opt {
                // R63 (Keycloak/Quarkus): `find_class_source_path` returns the
                // classpath entry verbatim — which may be a relative path like
                // `lib/quarkus-run.jar` when launched via `cd <appdir> && --jar
                // lib/quarkus-run.jar`.  Without canonicalization, the resulting
                // URL `file:lib/quarkus-run.jar` is opaque (no `/` after `file:`),
                // and Quarkus's `Path.of(QuarkusEntryPoint.class.getProtectionDomain()
                // .getCodeSource().getLocation().toURI()).getParent()` produces a
                // single-segment Path whose `getParent()` is null — exactly the
                // QuarkusEntryPoint.doRun:67 NPE.  Canonicalize so the URL is
                // always absolute (`file:/C:/.../lib/quarkus-run.jar`).
                // `class_code_base` is already a URL (`file:/...` or
                // `jar:file:/...`), while the legacy fallback above returns a
                // filesystem path. Normalize both before canonicalizing and
                // constructing the URL object below.
                let raw_path = raw_path
                    .strip_prefix("jar:file:")
                    .or_else(|| raw_path.strip_prefix("file:"))
                    .unwrap_or(&raw_path);
                let abs_path = std::fs::canonicalize(raw_path)
                    .ok()
                    .and_then(|p| p.to_str().map(|s| s.to_string()))
                    .map(|s| s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
                    .unwrap_or_else(|| raw_path.to_string());
                // Normalise backslashes to forward slashes so Path/File code
                // further up the stack works the same on every platform.
                let fwd = abs_path.replace('\\', "/");
                // Ensure absolute-style leading slash for Windows drive paths
                // (e.g. `C:/craton/...` → `/C:/craton/...`), matching the
                // URL-encoded form HotSpot produces.
                let path = if fwd.len() >= 2 && &fwd[1..2] == ":" {
                    format!("/{fwd}")
                } else if !fwd.starts_with('/') {
                    // Non-Windows relative path that we couldn't canonicalize —
                    // prefix with `/` so the URL is hierarchical (file:/...) and
                    // `Path.of(uri).getParent()` returns a real parent.
                    format!("/{fwd}")
                } else {
                    fwd
                };
                // Allocate a real `java.net.URL` and populate the JDK 25
                // instance-field layout so both real-JDK URL methods and our
                // native shims resolve a usable `file:/<path>` URL.
                //
                // Real JDK URL fields (declaration order, matching reflection):
                //   0=protocol, 1=host, 2=port (int), 3=file, 4=query,
                //   5=authority, 6=path, 7=userInfo, 8=ref,
                //   9=hostAddress, 10=handler, 11=hashCode (int), 12=tempState.
                //
                // Spring Boot 3.2's fat-jar launcher path takes
                // `pd.getCodeSource().getLocation().toURI().getSchemeSpecificPart()`
                // and feeds it to `new File(...)`. For that round-trip to
                // produce a path the Windows File ctor can resolve, we need
                // `URL.toString()` to emit `file:/<path>`, which real-JDK code
                // builds from `protocol + ":" + file`. We deliberately leave
                // `authority` (slot 5) null so URL.toURI doesn't take its
                // `isBuiltinStreamHandler(handler)` branch and NPE on the
                // null `handler` slot.
                //
                // gc-common w20-c: every allocation here can collect, and the
                // URL was written after three string allocations and stored
                // into the CodeSource after its allocation (each string was
                // held across the next one too). Allocate the strings first
                // and pin each value across the allocations that follow it.
                // SBR wave 2 (lane `netaddr`): `--real-jdk` runs the URL's own
                // JDK bodies (`toString` is `handler.toExternalForm(this)`), so
                // the location is built by the JDK constructor and carries
                // `sun.net.www.protocol.file.Handler`, as HotSpot's does.
                // `--compatible` keeps the slot-written carrier below.
                let url = match crate::jboss_module_loader::jdk_only_constructed_url(
                    ctx,
                    &format!("file:{path}"),
                ) {
                    Some(url) => url,
                    None => {
                        let path_str = ctx.create_string_uninterned(&path);
                        let strings_pin = ctx.pin_native_root(path_str);
                        let proto_str = ctx.create_string("file");
                        let proto_pin = ctx.pin_native_root(proto_str);
                        let host_str = ctx.create_string("");
                        let host_pin = ctx.pin_native_root(host_str);
                        let url = try_alloc_concurrent_synthetic(ctx, "java/net/URL", 13)?;
                        let path_str = ctx.read_native_pin(strings_pin, path_str);
                        let proto_str = ctx.read_native_pin(proto_pin, proto_str);
                        let host_str = ctx.read_native_pin(host_pin, host_str);
                        ctx.unpin_native_roots(strings_pin);
                        ctx.set_field(url, 0, Value::Object(Some(proto_str))); // protocol
                        ctx.set_field(url, 1, Value::Object(Some(host_str))); // host
                        ctx.set_field(url, 2, Value::Int(-1)); // port: -1 = unspecified
                        ctx.set_field(url, 3, Value::Object(Some(path_str))); // file
                        ctx.set_field(url, 6, Value::Object(Some(path_str))); // path
                        crate::jboss_module_loader::url_write_declared_field_defaults(&*ctx, url);
                        url
                    }
                };
                let url_pin = ctx.pin_native_root(url);
                let cs = try_alloc_concurrent_synthetic(ctx, "java/security/CodeSource", 2)?;
                let url = ctx.read_native_pin(url_pin, url);
                ctx.unpin_native_roots(url_pin);
                // BY NAME. These were raw slots 0 and 1; slot 0 is `location`
                // on both layouts, but slot 1 is `signers` on a real
                // `CodeSource` and only `certs` in the fabricated model — the
                // constructor-order mistake this file already records one
                // instance of, four lines below, for `ProtectionDomain`.
                ctx.set_field_by_name(cs, "location", Value::Object(Some(url)));
                ctx.set_field_by_name(cs, "certs", Value::Object(None));
                Value::Object(Some(cs))
            } else {
                Value::Object(None)
            };
            // SBR-13: populate `classloader` (the class's real defining loader,
            // matching `Class.getClassLoader()`) and a non-null (empty)
            // `permissions` collection. The previous code allocated only the
            // codesource + a Permissions on slot 1 — but slot 1 is the
            // `classloader` field in the real-JDK `ProtectionDomain` layout, so
            // `getClassLoader()` returned the Permissions object and
            // `getPermissions()` was null.
            // gc-common w20-c: the loader lookup can build a mirror (an
            // allocation), and `codesource` is stored after it.
            let codesource_pin = match codesource {
                Value::Object(Some(cs)) => Some((ctx.pin_native_root(cs), cs)),
                _ => None,
            };
            let classloader = lang_class::native_class_get_class_loader(ctx, args)
                .ok()
                .flatten()
                .unwrap_or(Value::Object(None));
            let codesource = match codesource_pin {
                Some((pin, cs)) => {
                    let cs = ctx.read_native_pin(pin, cs);
                    ctx.unpin_native_roots(pin);
                    Value::Object(Some(cs))
                }
                None => codesource,
            };
            // Read `pd` back from its pin — the whole point of taking it. Every
            // allocation above is behind us, and `populate_protection_domain_fields`
            // must write through the LIVE address, not the one this closure
            // started with.
            let pd = ctx.read_native_pin(pd_pin, pd);
            let populated =
                lang_class::populate_protection_domain_fields(ctx, pd, codesource, classloader);
            let pd = ctx.read_native_pin(pd_pin, pd);
            ctx.unpin_native_roots(pd_pin);
            populated?;
            Ok(Some(Value::Object(Some(pd))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // The compatibility registration above predates per-loader class
    // provenance and resolves same-named classes through the global classpath.
    // Keep it for its historic boot fallback behavior, then make the public
    // surface class-id-backed so a locally defined class reports its own JAR.
    registry.register(
        "java/lang/Class",
        "getProtectionDomain",
        "()Ljava/security/ProtectionDomain;",
        lang_class::native_class_get_protection_domain, cratonvm_native_api::NativeKind::Bridge,
    );
    // ProtectionDomain.getCodeSource() → return field 0
    registry.register(
        "java/security/ProtectionDomain",
        "getCodeSource",
        "()Ljava/security/CodeSource;",
        |ctx, args| match args.first() {
            Some(Value::Object(Some(pd))) => {
                let cs = ctx.get_field(*pd, 0);
                Ok(Some(cs))            }
            _ => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // W4: was a hard `true`. CratonVM does have a policy core — the parsed
    // `java.policy` grant table in `security_manager.rs` that backs both
    // `SecurityManager.checkPermission` and `Policy.implies(pd, perm)` — so
    // ask the same question those two ask and keep the three in lock-step.
    // With no `-Djava.security.policy` loaded the core is allow-all, i.e.
    // exactly the old constant; with one loaded the answer now reflects the
    // grants instead of rubber-stamping them.
    registry.register(
        "java/security/ProtectionDomain",
        "implies",
        "(Ljava/security/Permission;)Z",
        native_protection_domain_implies, cratonvm_native_api::NativeKind::Bridge,
    );
    // CodeSource.getLocation() → return field 0 (our synthetic URL)
    registry.register(
        "java/security/CodeSource",
        "getLocation",
        "()Ljava/net/URL;",
        |ctx, args| match args.first() {
            Some(Value::Object(Some(cs))) => Ok(Some(ctx.get_field(*cs, 0))),
            _ => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `CodeSource.getCertificates()` is `certs == null ? null : certs.clone()`.
    // NULL is the answer for a code source with no certificates, and it is what
    // `new CodeSource(url, (Certificate[]) null)` produces -- which is the
    // common shape. This fabricated an EMPTY ARRAY instead, so a caller's
    // `if (certs != null)` took the wrong branch and then found nothing in it.
    //
    // The array was also an `Object[]` in a slot declared
    // `[Ljava/security/cert/Certificate;`, which nothing inside this VM
    // objects to and real bytecode does -- the same descriptor mismatch that
    // made `Throwable.suppressedExceptions` unserialisable. Reading the field
    // avoids inventing either. MEASURED by
    // `SecuritySurfaceSweep.java`.
    registry.register(
        "java/security/CodeSource",
        "getCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| match args.first() {
            Some(Value::Object(Some(cs))) => Ok(Some(ctx.get_field_by_name(*cs, "certs"))),
            _ => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // URL.getPath() — prefer real-JDK 13-field layout (path@6, file@3,
    // protocol@0). The earlier synthetic 1-field layout (path@0) is detected
    // by checking object_num_fields == 1, in which case slot 0 holds the path
    // string. For multi-field URLs, read by name so we get the path, not the
    // protocol. R63 (Keycloak): without this, `URL.getPath()` on a PD URL
    // returns "file" instead of "/C:/.../quarkus-run.jar", and Quarkus's
    // `Path.of(URLDecoder.decode(getPath(), UTF_8)).getParent().getParent().getParent()`
    // NPEs at QuarkusEntryPoint.doRun:67.
    fn url_path_or_file_field(
        ctx: &mut dyn NativeContext,
        url: ObjectRef,
        prefer_file: bool,
    ) -> Value {
        let n = ctx.object_num_fields(url);
        if n == 1 {
            // legacy synthetic URL: slot 0 holds the path String.
            return ctx.get_field(url, 0);
        }
        let named_first = if prefer_file { "file" } else { "path" };
        let named_second = if prefer_file { "path" } else { "file" };
        let slot_first = if prefer_file { 3 } else { 6 };
        let slot_second = if prefer_file { 6 } else { 3 };

        let v = ctx.get_field_by_name(url, named_first);
        if let Value::Object(Some(_)) = v {
            return v;
        }
        if n > slot_first {
            let v = ctx.get_field(url, slot_first);
            if let Value::Object(Some(_)) = v {
                return v;
            }
        }
        let v = ctx.get_field_by_name(url, named_second);
        if let Value::Object(Some(_)) = v {
            return v;
        }
        if n > slot_second {
            let v = ctx.get_field(url, slot_second);
            if let Value::Object(Some(_)) = v {
                return v;
            }
        }
        Value::Object(None)
    }
    registry.register(
        "java/net/URL",
        "getPath",
        "()Ljava/lang/String;",
        |ctx, args| match args.first() {
            Some(Value::Object(Some(url))) => Ok(Some(url_path_or_file_field(ctx, *url, false))),
            _ => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/net/URL",
        "getFile",
        "()Ljava/lang/String;",
        |ctx, args| match args.first() {
            Some(Value::Object(Some(url))) => Ok(Some(url_path_or_file_field(ctx, *url, true))),
            _ => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // URL.getProtocol() / getHost() — use real-JDK URL field layout
    // (protocol=0, host=1). The legacy versions of these natives indexed
    // the wrong slots, which made Keycloak's SmallRye config loader throw
    // "Unexpected protocol null for URL jar:file:..." when scanning the
    // classpath. The named field lookup is layout-neutral.
    registry.register(
        "java/net/URL",
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| match args.first() {
            Some(Value::Object(Some(url))) => {
                let v = ctx.get_field_by_name(*url, "protocol");
                if let Value::Object(Some(_)) = v {
                    return Ok(Some(v));                }
                Ok(Some(Value::Object(Some(ctx.create_string("file")))))            }
            _ => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/net/URL",
        "getHost",
        "()Ljava/lang/String;",
        |ctx, args| match args.first() {
            Some(Value::Object(Some(url))) => {
                let v = ctx.get_field_by_name(*url, "host");
                if let Value::Object(Some(_)) = v {
                    return Ok(Some(v));                }
                Ok(Some(Value::Object(Some(ctx.create_string("")))))            }
            _ => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // URL.equals / URL.sameFile / URL.hashCode — based on the URL string to avoid
    // NPE from null `handler` in synthetic URL objects, and to ensure correct
    // deduplication when the same resource URL appears from multiple classloaders
    // (e.g. AggregatedClassLoader iterating N loaders all returning the same
    // persistence.xml path — LinkedHashSet<URL> must deduplicate them).
    registry.register(
        "java/net/URL",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_url_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/net/URL", "hashCode", "()I", native_url_hash_code, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/net/URL",
        "sameFile",
        "(Ljava/net/URL;)Z",
        native_url_same_file, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.lang.Thread (native methods) ---
    // JNI symbol binding only — see java/lang/Object.registerNatives above.
    registry.register(
        "java/lang/Thread",
        "registerNatives",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    // nio21 wave 3 L1 (`r21-current-thread-jdk-only-needs-a-classified-gate`):
    // a reviewed `Intrinsic`, stated here, not inherited. JDK 25 declares
    // `public static native Thread currentThread()` (`@IntrinsicCandidate`),
    // so no bytecode competes with the registration, and the body is
    // HotSpot's `_currentThread` intrinsic exactly: one read of the running
    // thread's mirror (built on first call, as HotSpot builds it at attach).
    // `Intrinsic` is what lets the strict thin-helper gate admit it without
    // the enforcement dial's per-class question. NOT a synthetic stub, so
    // `the native-registry census` does not move; the kind map row is `intrinsic`.
    // `CRATONVM_THREAD_CURRENT_THREAD_INTRINSIC=0`: the pre-wave-3 `Bridge`.
    registry.register(
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;",
        native_thread_current_thread,
        thread_current_thread_kind(),
    );
    registry.register(
        "java/lang/Thread",
        "sleep",
        "(J)V",
        crate::lang_system::native_thread_sleep_jfr, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/lang/Thread", "yield", "()V", |_ctx, _args| {
        std::thread::yield_now();
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/Thread",
        "interrupt0",
        "()V",
        native_thread_interrupt,
        NativeKind::Bridge,
    );
    // Also register public interrupt() so it works on synthetic Thread stubs
    registry.register(
        "java/lang/Thread",
        "interrupt",
        "()V",
        native_thread_interrupt, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Thread.isInterrupted()` / `Thread.interrupted()` (JDK 25): these are pure
    // bytecode reading the Java `interrupted` field. But CratonVM's interrupt path
    // (`native_thread_interrupt`) records the interrupt only in the VM's
    // ThreadRegistry flag, never the Java field — so in real-JDK mode the field
    // read always returns `false` and AQS interrupt checks (`ConditionObject.await`
    // polling `Thread.interrupted()`, etc.) never observe an interrupt, hanging
    // executor workers on `shutdownNow()`. Shadow both with natives that read the
    // registry flag — the single source of truth every native blocking op
    // (sleep0/wait/join/park) already uses. (`register_synthetic_overrides`
    // registers equivalent closures for synthetic-JDK mode after this.)
    //
    // STALE PREMISE since SBR wave 6: the VM now writes the `interrupted` FIELD
    // wherever it raises or consumes its flag (`vm_exec::
    // set_thread_interrupted_field`), which is why `--real-jdk` runs the JDK
    // bodies of `interrupt` / `isInterrupted` / `interrupted`. Over a real JDK
    // `--compatible` drops these three registrations too with the
    // `jdk-thread-interrupt` HotSpot-alignment row (nio21 wave 3 L1, default
    // ON; `retired_shadow::is_thread_interrupt_family_row`), so the closures
    // below serve only `CRATONVM_COMPAT_ALIGN=-jdk-thread-interrupt`.
    registry.register("java/lang/Thread", "isInterrupted", "()Z", |ctx, args| {
        // Instance method: read the RECEIVER's interrupt status (which may be a
        // thread *other* than the caller — e.g. `ThreadPoolExecutor`).
        let interrupted = match args.first() {
            Some(Value::Object(Some(this))) => {
                if ctx.thread_is_interrupted(*this) {
                    true
                } else if ctx.thread_run_state(*this) == 0 {
                    // NEW — never started, so the thread registry has no entry
                    // for it and `ctx.thread_interrupt` had nowhere to record
                    // the request. The real `Thread.interrupt()` writes the
                    // Java `interrupted` FIELD before it tells the VM, and
                    // `native_thread_interrupt` already mirrors that write, so
                    // the field is the ONLY record of an interrupt delivered to
                    // a thread that has not run. MEASURED, both modes
                    // (`probes/ThreadIntrDbg.java`):
                    //
                    //   new Thread(r); t.interrupt(); t.isInterrupted()
                    //     HotSpot  true      CratonVM  false
                    //
                    // and the status has to SURVIVE the start, because
                    // `Thread.start()` does not clear it: HotSpot answers true
                    // again after the thread has run and finished.
                    //
                    // Scoped to state 0 deliberately. For a started thread the
                    // registry is the single source of truth every native
                    // blocking op already clears through, and OR-ing the field
                    // in unconditionally would resurrect a status that
                    // `Thread.interrupted()` had just cleared — the row
                    // `isInterrupted after interrupted() cleared` measures
                    // exactly that, and it passes today.
                    // Round 15 wave 4 (lane threadvm): `Thread`'s own field
                    // (`lang_misc::thread_field`), which the interrupt writes.
                    matches!(
                        crate::lang_misc::thread_field(&*ctx, *this, "interrupted"),
                        Value::Int(v) if v != 0
                    )
                } else {
                    false
                }
            }
            _ => ctx.is_interrupted(false),
        };
        Ok(Some(Value::Int(if interrupted { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // LEAF: the body is one `Acquire` load of THIS thread's own `interrupted`
    // flag and, when set, one `Release` store clearing it
    // (`NativeContextImpl::is_interrupted`). No argument to pin, nothing that
    // allocates, blocks, safepoints or throws — `NativeMethodRegistry::set_leaf`'s
    // four terms hold on every path, including the `false` one.
    //
    // Worth claiming because AQS polls it: the exact census configuration
    // (`--nojit CRATONVM_DISABLE_INTRINSICS=1`) puts it at **1.69 calls per
    // expired task** on `HashedWheelTimerTest#testExecutionOnTime`, third
    // behind `Thread.currentThread` and `setExclusiveOwnerThread`. Measured
    // from compiled code at 55.8 ns/call against HotSpot's 0.20
    // (`probes/FunnelRungRate.java`), essentially all of it the funnel.
    //
    // The INSTANCE twin `isInterrupted()` above is deliberately NOT claimed:
    // its cross-thread arm resolves the target through the registry's
    // `java_tid_to_id` mutex, which leaf contract item 2 forbids, and it is
    // 0.27 calls per task — a sixth of the traffic for a term that would have
    // to be argued rather than read off the body.
    registry.set_leaf(true);
    registry.register("java/lang/Thread", "interrupted", "()Z", |ctx, _args| {
        // Static method: read+clear the CURRENT thread's interrupt status.
        Ok(Some(Value::Int(if ctx.is_interrupted(true) {
            1
        } else {
            0
        })))
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.set_leaf(false);
    // getPriority / isDaemon / setDaemon: in real-JDK mode these live on
    // `Thread.holder` (a `Thread$FieldHolder`).  These natives shadow the
    // real Java methods, so they must read/write through `holder` when it
    // is present.  When `holder` is null (synthetic-JDK Thread, or a
    // not-fully-constructed Thread) they fall back to spec defaults.
    // Round 15 wave 3 (lane trace4): `holder` is the field `java.lang.Thread`
    // declares (`lang_misc::thread_field`), not a subclass's of that name.
    registry.register("java/lang/Thread", "getPriority", "()I", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            if let Value::Object(Some(holder)) =
                crate::lang_misc::thread_field(ctx, *this, "holder")
            {
                if let Value::Int(p) = ctx.get_field_by_name(holder, "priority") {
                    return Ok(Some(Value::Int(p)));
                }
            }
        }
        Ok(Some(Value::Int(5)))
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/lang/Thread", "isDaemon", "()Z", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            if let Value::Object(Some(holder)) =
                crate::lang_misc::thread_field(ctx, *this, "holder")
            {
                if let Value::Int(d) = ctx.get_field_by_name(holder, "daemon") {
                    return Ok(Some(Value::Int(if d != 0 { 1 } else { 0 })));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // `setDaemon` is THREE guards and then a write, and this body had none of
    // them. MEASURED against HotSpot 25.0.4+7, both modes:
    //
    //   t.start(); t.setDaemon(true)            HotSpot IllegalThreadStateException
    //   Thread.ofVirtual().unstarted(r)
    //        .setDaemon(false)                  HotSpot IllegalArgumentException
    //
    // ```java
    //   public final void setDaemon(boolean on) {
    //       if (isVirtual() && !on)
    //           throw new IllegalArgumentException("'false' not legal for virtual threads");
    //       if (isAlive())
    //           throw new IllegalThreadStateException();
    //       if (!isVirtual())
    //           daemon = on;
    //   }
    // ```
    //
    // The ORDER is load-bearing and is the JDK's: the virtual-thread refusal
    // comes FIRST, so `ofVirtual().unstarted(r).setDaemon(false)` is an
    // `IllegalArgumentException` and not the `IllegalThreadStateException` an
    // is-alive-first reading would give a STARTED virtual thread. The
    // is-alive guard matters most: the daemon flag is read once, when the
    // thread starts, so writing it afterwards was a silent no-op that told the
    // caller its non-daemon thread was now a daemon — and a JVM that will not
    // exit is exactly what that caller was trying to avoid.
    //
    // `IllegalThreadStateException` carries an EMPTY message, which this
    // variant renders as NO message rather than `""` (see `error.rs`, the same
    // distinction `Thread.start()`'s guard needed in G72-1).
    registry.register("java/lang/Thread", "setDaemon", "(Z)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let this = *this;
            let on = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
            // gen r4w3/rooting: `isVirtual` is a Java upcall (GC point);
            // `this` is used after it, so pin and re-read.
            let this_pin = ctx.pin_native_root(this);
            let virtual_thread = matches!(
                ctx.invoke_virtual(this, "isVirtual", "()Z", &[]),
                Ok(Some(Value::Int(v))) if v != 0
            );
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            if virtual_thread && !on {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "'false' not legal for virtual threads".to_string(),
                }
                .into());
            }
            if ctx.thread_is_alive(this) {
                return Err(RuntimeError::IllegalThreadStateException {
                    message: String::new(),
                }
                .into());
            }
            if virtual_thread {
                return Ok(None);
            }
            if let Value::Object(Some(holder)) = crate::lang_misc::thread_field(ctx, this, "holder")
            {
                ctx.set_field_by_name(holder, "daemon", Value::Int(if on { 1 } else { 0 }));            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/Thread",
        "getName",
        "()Ljava/lang/String;",
        crate::lang_system::native_thread_get_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "setName",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first() else {
                return Ok(None);
            };
            // `Thread.setName`'s first statement is
            // `if (name == null) throw new NullPointerException("name cannot be
            // null")`. Accepting the null was TWO defects, not one: the throw
            // never happened, and the null was then written into `name`, so
            // `getName()` answered null afterwards -- a Thread whose name is
            // null is a state the JDK's own API cannot produce. MEASURED:
            // `probes/ReflectBufferSweep.java`, the single difference in 316
            // assertions across four families, in BOTH modes.
            let name = match args.get(1) {
                Some(v @ Value::Object(Some(_))) => v.clone(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("name cannot be null".to_string()),
                    }
                    .into())
                }
            };
            // Round 15 wave 3 (lane trace4): slot 0 is `name` in the SYNTHETIC
            // layout only; on the real one (more than 8 slots) it is whichever
            // `java.lang.Thread` field comes first, which the `String`
            // clobbered. `Thread`'s own `name` is written too: by name
            // a subclass's `name` field took it and `Thread.name` kept the old
            // name.
            let num_fields = ctx.object_num_fields(*this);
            let raw_slot_ok =
                is_synthetic_thread_layout(num_fields) || !thread_raw_slots_synthetic_only();
            if num_fields > 0 && raw_slot_ok {
                ctx.set_field(*this, 0, name.clone());
            }
            // Round 15 wave 4 (lane threadvm): `Thread`'s own `name` only
            // (`lang_misc::set_thread_field`); a subclass's `name` field is
            // left alone, as HotSpot's `setName` leaves it.
            crate::lang_misc::set_thread_field(ctx, *this, "name", name.clone());
            if !is_synthetic_thread_layout(num_fields)
                && crate::lang_misc::thread_declared_fields_on()
            {
                if let Some(name_slot) = ctx.resolve_field_index("java/lang/Thread", "name") {
                    ctx.set_field(*this, name_slot, name);                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Thread.{set,get}[Default]UncaughtExceptionHandler — the real
    // implementation lives in `uncaught_handlers`, which stores the handler in
    // a GC-rooted side table that `vm_exec.rs::thread_start` consults when a
    // thread dies with an uncaught exception. It used to be preceded here by
    // noop/null-returning placeholders for all four methods; because
    // `register` is last-wins those were pure dead weight (the very next line
    // overwrote them) while reading as if handlers were silently dropped.
    crate::uncaught_handlers::register_uncaught_handler_natives(registry);
    registry.register(
        "java/lang/Thread",
        "getThreadGroup",
        "()Ljava/lang/ThreadGroup;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let num_fields = ctx.object_num_fields(this);
            // Round 15 wave 5 (lane trace5; `r15w3-trace4-review-findings` item
            // 1): JDK 25 is `isTerminated() ? null : isVirtual() ?
            // virtualThreadGroup() : holder.group`. Real layout only.
            let hotspot_rule =
                !is_synthetic_thread_layout(num_fields);
            if hotspot_rule && ctx.thread_run_state(this) == 2 {
                return Ok(Some(Value::Object(None)));
            }
            if !is_synthetic_thread_layout(num_fields) {
                // Round 15 wave 3 (lane trace4): `Thread`'s own `holder`.
                if let Value::Object(Some(holder)) =
                    crate::lang_misc::thread_field(ctx, this, "holder")
                {
                    let group = ctx.get_field_by_name(holder, "group");
                    if !matches!(group, Value::Object(None)) {
                        return Ok(Some(group));
                    }
                }
            }
            // A virtual thread has no `holder` (a bound one has, with this
            // group in it).
            if hotspot_rule && is_virtual_thread_object(ctx, this) {
                return Ok(Some(virtual_thread_group(ctx)?));
            }
            // Round 15 wave 3 (lane trace4): slot 2 is the group in the
            // SYNTHETIC layout only; on the real one (a virtual thread has no
            // `holder`) it is another `Thread` field, which was returned as a
            // `ThreadGroup`.
            let raw_slot_ok =
                is_synthetic_thread_layout(num_fields) || !thread_raw_slots_synthetic_only();
            let group = if num_fields > 2 && raw_slot_ok {
                ctx.get_field(this, 2)
            } else {
                Value::Object(None)
            };
            Ok(Some(match group {
                Value::Object(Some(_)) => group,
                _ => Value::Object(None),
            }))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/lang/Thread", "isAlive", "()Z", native_thread_is_alive, cratonvm_native_api::NativeKind::Bridge);
    // JDK 25 additional Thread natives:
    registry.register(
        "java/lang/Thread",
        "currentCarrierThread",
        "()Ljava/lang/Thread;",
        native_jla_current_carrier_thread,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread$FieldHolder",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;JIZ)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let task = args.get(2).cloned().unwrap_or(Value::Object(None));
            let stack_size = args.get(3).cloned().unwrap_or(Value::Long(0));
            let priority = args.get(4).cloned().unwrap_or(Value::Int(5));
            let daemon = args.get(5).cloned().unwrap_or(Value::Int(0));

            ctx.set_field_by_name(this, "group", group.clone());
            ctx.set_field_by_name(this, "task", task.clone());
            ctx.set_field_by_name(this, "stackSize", stack_size.clone());
            ctx.set_field_by_name(this, "priority", priority.clone());
            ctx.set_field_by_name(this, "daemon", daemon.clone());
            ctx.set_field_by_name(this, "threadStatus", Value::Int(0));

            if ctx.object_num_fields(this) >= 6 {
                ctx.set_field(this, 0, group);
                ctx.set_field(this, 1, task);
                ctx.set_field(this, 2, stack_size);
                ctx.set_field(this, 3, priority);
                ctx.set_field(this, 4, daemon);
                ctx.set_field(this, 5, Value::Int(0));            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/lang/Thread", "<init>", "()V", |ctx, args| {
        let mut this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        crate::lang_system::capture_inheritable_tl_at_construction(ctx, &mut this);
        let name = Value::Object(Some(thread_default_name_rooted(ctx, &mut this, &mut [])));
        if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
            populate_real_thread_holder(ctx, this, Value::Object(None), Value::Object(None), name);
            return Ok(None);
        }
        ctx.set_field(this, 0, name);
        ctx.set_field(this, 1, Value::Int(5));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            // gen r4w3/rooting: the capture runs `childValue` bytecode (a GC
            // point); root the group/target/name args across it and read them
            // back from the refreshed copy, never from the stale `args` slice.
            let mut refreshed_args = thread_init_capture_itl_rooted(ctx, &mut this, args);
            let name = Value::Object(Some(thread_default_name_rooted(
                ctx,
                &mut this,
                &mut refreshed_args,
            )));
            let args: &[Value] = &refreshed_args;
            let target = args.get(1).cloned().unwrap_or(Value::Object(None));
            if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
                populate_real_thread_holder(ctx, this, Value::Object(None), target, name);
                return Ok(None);
            }
            ctx.set_field(this, 0, name);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            thread_reject_null_name(args.get(1))?;
            // gen r4w3/rooting: the capture runs `childValue` bytecode (a GC
            // point); root the group/target/name args across it and read them
            // back from the refreshed copy, never from the stale `args` slice.
            let refreshed_args = thread_init_capture_itl_rooted(ctx, &mut this, args);
            let args: &[Value] = &refreshed_args;
            let name = match args.get(1).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
                populate_real_thread_holder(
                    ctx,
                    this,
                    Value::Object(None),
                    Value::Object(None),
                    name,
                );
                return Ok(None);
            }
            ctx.set_field(this, 0, name);
            ctx.set_field(this, 1, Value::Int(5));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            thread_reject_null_name(args.get(2))?;
            // gen r4w3/rooting: the capture runs `childValue` bytecode (a GC
            // point); root the group/target/name args across it and read them
            // back from the refreshed copy, never from the stale `args` slice.
            let refreshed_args = thread_init_capture_itl_rooted(ctx, &mut this, args);
            let args: &[Value] = &refreshed_args;
            let target = args.get(1).cloned().unwrap_or(Value::Object(None));
            let name = match args.get(2).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
                populate_real_thread_holder(ctx, this, Value::Object(None), target, name);
                return Ok(None);
            }
            ctx.set_field(this, 0, name);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;)V",
        |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            // gen r4w3/rooting: the capture runs `childValue` bytecode (a GC
            // point); root the group/target/name args across it and read them
            // back from the refreshed copy, never from the stale `args` slice.
            let mut refreshed_args = thread_init_capture_itl_rooted(ctx, &mut this, args);
            let name_val = Value::Object(Some(thread_default_name_rooted(
                ctx,
                &mut this,
                &mut refreshed_args,
            )));
            let args: &[Value] = &refreshed_args;
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let target = args.get(2).cloned().unwrap_or(Value::Object(None));
            if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
                populate_real_thread_holder(ctx, this, group, target, name_val);
                return Ok(None);
            }
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V",
        |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            thread_reject_null_name(args.get(2))?;
            // gen r4w3/rooting: the capture runs `childValue` bytecode (a GC
            // point); root the group/target/name args across it and read them
            // back from the refreshed copy, never from the stale `args` slice.
            let refreshed_args = thread_init_capture_itl_rooted(ctx, &mut this, args);
            let args: &[Value] = &refreshed_args;
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let name_val = match args.get(2).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
                populate_real_thread_holder(ctx, this, group, Value::Object(None), name_val);
                return Ok(None);
            }
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
        |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            thread_reject_null_name(args.get(3))?;
            // gen r4w3/rooting: the capture runs `childValue` bytecode (a GC
            // point); root the group/target/name args across it and read them
            // back from the refreshed copy, never from the stale `args` slice.
            let refreshed_args = thread_init_capture_itl_rooted(ctx, &mut this, args);
            let args: &[Value] = &refreshed_args;
            if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
                // Real-JDK Thread: this native intercepts the real Java
                // constructor, so we must populate the `holder:FieldHolder`
                // it would otherwise build (see populate_real_thread_holder).
                let group = args.get(1).cloned().unwrap_or(Value::Object(None));
                let target = args.get(2).cloned().unwrap_or(Value::Object(None));
                let name_val = args
                    .get(3)
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Some(thread_default_name(ctx))));
                populate_real_thread_holder(ctx, this, group, target, name_val);
                return Ok(None);
            }
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let target = args.get(2).cloned().unwrap_or(Value::Object(None));
            let name_val = args
                .get(3)
                .cloned()
                .unwrap_or_else(|| Value::Object(Some(thread_default_name(ctx))));
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;J)V",
        |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            thread_reject_null_name(args.get(3))?;
            // gen r4w3/rooting: the capture runs `childValue` bytecode (a GC
            // point); root the group/target/name args across it and read them
            // back from the refreshed copy, never from the stale `args` slice.
            let refreshed_args = thread_init_capture_itl_rooted(ctx, &mut this, args);
            let args: &[Value] = &refreshed_args;
            if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
                let group = args.get(1).cloned().unwrap_or(Value::Object(None));
                let target = args.get(2).cloned().unwrap_or(Value::Object(None));
                let name_val = match args.get(3).cloned().unwrap_or(Value::Object(None)) {
                    Value::Object(Some(s)) => Value::Object(Some(s)),
                    _ => Value::Object(Some(thread_default_name(ctx))),
                };
                // `long stackSize` is ONE slot of `args` (see the `JZ`
                // twin below), at index 4.
                let stack_size = match args.get(4) {
                    Some(Value::Long(n)) => *n,
                    _ => 0,
                };
                populate_real_thread_holder_sized(ctx, this, group, target, name_val, stack_size);
                return Ok(None);
            }
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let target = args.get(2).cloned().unwrap_or(Value::Object(None));
            let name_val = match args.get(3).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;JZ)V",
        |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            thread_reject_null_name(args.get(3))?;
            // This is the ONE public constructor that can opt OUT of
            // inheritance: `inheritThreadLocals == false` becomes
            // `characteristics |= NO_INHERIT_THREAD_LOCALS` (4), and the JDK
            // master constructor's `iand`/`ifne` at pc 169..172 then skips the
            // copy entirely. MEASURED on HotSpot 25.0.3+9
            // (`ItlProbe` case 11): `new Thread(g, r, n, 0, false)` has the
            // child read `null` even though the parent had a value set.
            //
            // Queue an EMPTY capture rather than skipping the call: an absent
            // queue entry makes `inheritable_tl_captured_at_construction`
            // answer false, and `native_thread_start0` would then fall back to
            // snapshotting the parent's CURRENT map — i.e. opting out would
            // hand the child MORE than opting in. The empty entry is the same
            // shape `capture_inheritable_tl_at_construction` queues for a
            // parent with no inheritable values (`ItlProbe` case 9).
            //
            // Slot 5 is the boolean: a `J` occupies ONE slot in this `args`
            // vec, not two — SOURCE-VERIFIED against
            // `Unsafe.putBoolean(Object,long,boolean)`, whose body
            // (`unsafe_natives_ext.rs:3136`) reads the receiver at 1, the
            // offset at 2 and the boolean at 3. Booleans arrive as
            // `Value::Int`. The match is deliberately exact: anything that is
            // not a literal `Int(0)` takes the ordinary capture path.
            let refreshed_args: Vec<Value>;
            let args: &[Value] = if matches!(args.get(5), Some(Value::Int(0))) {
                crate::phases_early::queue_inherited_tl_for_child(ctx, this, Default::default());
                args
            } else {
                // gen r4w3/rooting: the capture runs `childValue` bytecode (a
                // GC point); root the group/target/name args across it and
                // read them back from the refreshed copy.
                refreshed_args = thread_init_capture_itl_rooted(ctx, &mut this, args);
                &refreshed_args
            };
            if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
                let group = args.get(1).cloned().unwrap_or(Value::Object(None));
                let target = args.get(2).cloned().unwrap_or(Value::Object(None));
                let name_val = match args.get(3).cloned().unwrap_or(Value::Object(None)) {
                    Value::Object(Some(s)) => Value::Object(Some(s)),
                    _ => Value::Object(Some(thread_default_name(ctx))),
                };
                // `long stackSize` at index 4 (one slot), the boolean at 5.
                let stack_size = match args.get(4) {
                    Some(Value::Long(n)) => *n,
                    _ => 0,
                };
                // `inheritThreadLocals == false` also means the thread does
                // NOT inherit the creator's context class loader (the JDK
                // defaults it to the system loader instead).
                let inherit = !matches!(args.get(5), Some(Value::Int(0)));
                populate_real_thread_holder_full(
                    ctx, this, group, target, name_val, stack_size, inherit,
                );
                return Ok(None);
            }
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let target = args.get(2).cloned().unwrap_or(Value::Object(None));
            let name_val = match args.get(3).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "start0",
        "()V",
        native_thread_start0,
        NativeKind::Bridge,
    );
    // Thread.start: in real-JDK mode the JDK's Java implementation must
    // run (it sets thread state, checks already-started, and calls start0).
    // Only intercept for synthetic-JDK Thread where there is no Java body
    // worth executing.
    registry.register("java/lang/Thread", "start", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if !is_synthetic_thread_layout(ctx.object_num_fields(this)) {
            // Real-JDK Thread: spawn directly. The JDK's Thread.start()
            // does threadStatus checks then calls start0() native, but
            // since we've registered an override on "start" we bypass
            // that here. Just spawn via the same native.
            return native_thread_start0(ctx, args);
        }
        native_thread_start0(ctx, args)
    }, cratonvm_native_api::NativeKind::Bridge);
    // Thread.getState()/threadState(): real-JDK bytecode reads
    // holder.threadStatus, which the VM never advances past 0 → getState()
    // wrongly reports NEW even for finished threads, tripping strict
    // thread-leak detectors (ES RestClient's randomizedtesting suite). Compute
    // the state from the VM thread registry and return the canonical
    // Thread$State enum constant. (Synthetic-JDK keeps its own override.)
    registry.register(
        "java/lang/Thread",
        "getState",
        "()Ljava/lang/Thread$State;",
        crate::lang_system::native_thread_get_state, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "threadState",
        "()Ljava/lang/Thread$State;",
        crate::lang_system::native_thread_get_state, cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP: genuine no-op. `Thread.checkAccess()` only ever consulted the
    // SecurityManager, which was removed in JDK 24 — the real body is empty
    // (and the method is deprecated for removal).
    registry.register("java/lang/Thread", "checkAccess", "()V", |_ctx, _args| {
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // Thread.run: synthetic-JDK reads slot 3 = target. Real-JDK Thread.run
    // reads `holder.task` from FieldHolder. For real-JDK we resolve the
    // `target` field by name so we invoke the user's Runnable correctly.
    //
    // The `?` on the delegating call is load-bearing: it used to be
    // `let _ = ...`, which discarded `Err(ExceptionThrown)` and returned
    // `Ok(None)`. Since this shim is force-listed for `Thread.run()V` (see
    // `runtime/interpreter.rs`) it sits on `super.run()` for EVERY Thread
    // subclass, so that swallowed every uncaught exception thrown by every
    // Runnable in the VM: `vm_exec.rs::thread_start`'s `if let Err(e)` guard
    // never fired, which is why neither the UncaughtExceptionHandler nor even
    // the default `Exception in thread "..."` stderr report ever ran.
    // `Thread.run()` catches nothing in HotSpot — propagate.
    registry.register("java/lang/Thread", "run", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let num_fields = ctx.object_num_fields(this);
        if is_synthetic_thread_layout(num_fields) {
            if num_fields >= 4 {
                if let Value::Object(Some(target)) = ctx.get_field(this, 3) {
                    crate::lang_misc::served_thread_run_call(ctx, target)?;
                }
            }
        } else {
            // Real-JDK: target is stored on this Thread. Could be on `holder`
            // (FieldHolder.task) or directly on Thread depending on JDK
            // version. Round 15 wave 3 (lane trace3): the fields `Thread`
            // declares, not a subclass's of the same name.
            let target_val = crate::lang_misc::served_thread_run_task(ctx, this);
            if let Value::Object(Some(target)) = target_val {
                // Round 15 wave 3 (lane trace4; TR2W2-2): recorded for the
                // stack captures inside it.
                crate::lang_misc::served_thread_run_call(ctx, target)?;            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/Thread",
        "yield0",
        "()V",
        |_ctx, _args| {
            std::thread::yield_now();
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "sleepNanos0",
        "(J)V",
        native_thread_sleep_nanos,
        NativeKind::Bridge,
    );
    // T19.N2: JDK 21+ Thread.sleep(long) delegates to this private native
    // for the actual sleep + interrupt handling.
    registry.register("java/lang/Thread", "sleep0", "(J)V", native_thread_sleep0, cratonvm_native_api::NativeKind::Bridge);
    // T2.2.21: Thread.sleep(long millis, int nanos).
    registry.register(
        "java/lang/Thread",
        "sleep",
        "(JI)V",
        crate::lang_system::native_thread_sleep_millis_nanos_jfr, cratonvm_native_api::NativeKind::Bridge,
    );

    // T1.9.1 — `Reference.reachabilityFence(Object)`.
    //
    // Pins the argument across the call so the GC cannot collect it
    // before the fence returns, even if the JIT would otherwise prove
    // the local was dead. Implemented as a `core::hint::black_box` on
    // the argument: the optimizer is not allowed to reason about its
    // value, so any aliased GC root remains live across the boundary.
    registry.register(
        "java/lang/ref/Reference",
        "reachabilityFence",
        "(Ljava/lang/Object;)V",
        |_ctx, args| {
            let _ = std::hint::black_box(args.first().copied());
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // T1.5.1 — `Thread.stop0(Object)` / `Thread.stop()`: REGISTERED NOWHERE
    // IN THIS FILE, and that is a deletion, not an omission.
    //
    // Both triples used to be registered here AND in
    // `deprecated_lang.rs::register_deprecated_lang_natives`, which runs later
    // and therefore owned both slots. MEASURED from
    // `--dump-native-registry --explain-jdk-only --real-jdk`:
    //
    // ```text
    //   stop  ()V                  lib.rs  owns_slot=false   deprecated_lang.rs  owns_slot=true
    //   stop0 (Ljava/lang/Object;)V lib.rs  owns_slot=false   deprecated_lang.rs  owns_slot=true
    // ```
    //
    // So neither body here has ever run. That made them a LANDMINE rather than
    // dead weight: a census-driven retirement of the `deprecated_lang.rs`
    // registration — which is what the row counts recommend, and what
    // `docs/known-issues/jdk-only/H25-3-*.md` R1 was written to refuse —
    // would have PROMOTED the `stop()` closure that was here, turning a method
    // that correctly throws `UnsupportedOperationException` into one that
    // silently interrupts the target thread and returns normally. The census
    // would have fallen by one and the change would have scored as a win.
    //
    // `stop()` is now served by the real bytecode of every supported image
    // (JDK 21/25 throw `UnsupportedOperationException`; JDK 17 does the real
    // deprecated work through `stop0`). `stop0` keeps its single
    // `deprecated_lang.rs` registration, because JDK 17 declares it — see the
    // note there.

    // T1.6.7 — `Thread.holdsLock(Object)` real implementation.
    // Was a constant `return false` stub. Now consults the per-thread
    // monitor table via NativeContext::current_thread_holds_lock.
    // JIT tier2 wave 5, lane nat21 (NATIVE21-2): LEAF (audited) through
    // `native_thread_holds_lock_leaf`, which answers from the mark word and this
    // thread's caches and declines to `native_thread_holds_lock` on the full
    // funnel for everything else. The kind stays `Bridge`, stated.
    registry.with_leaf(true, |r| {
        r.register(
            "java/lang/Thread",
            "holdsLock",
            "(Ljava/lang/Object;)Z",
            native_thread_holds_lock_leaf,
            NativeKind::Bridge,
        );
    });
    registry.register(
        "java/lang/Thread",
        "setPriority0",
        "(I)V",
        |_ctx, _args| {
            // KEEP: genuine no-op, and the same one HotSpot performs. The Java-visible
            // state (`Thread.holder.priority`, what `getPriority()` returns) is written
            // by the `setPriority(int)` bytecode BEFORE this call; `setPriority0`'s only
            // job is the OS scheduler hint. HotSpot on Linux likewise drops it unless it
            // runs as root with `-XX:ThreadPriorityPolicy=1`, so "accept and ignore" is
            // the mainstream platform behaviour, not an unimplemented stub.
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "setNativeName",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            // W3: actually rename the OS thread. HotSpot's `set_native_thread_name`
            // is what makes `top -H` / gdb / the crash handler show Java thread
            // names, and the STW-census work depends on being able to name the
            // holdout thread. Only the CALLING thread can be renamed portably
            // (pthread_setname_np on another thread needs its pthread_t, which we
            // do not surface), so a rename of some *other* live thread stays a
            // no-op — the Java-side `Thread.name` field is written by the caller
            // either way.
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(None);
            };
            let Some(Value::Object(Some(name_obj))) = args.get(1).copied() else {
                return Ok(None);
            };
            let Some(name) = ctx.read_string(name_obj) else {
                return Ok(None);
            };
            if ctx.current_thread_object() == this {
                set_current_os_thread_name(&name);            }
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "getNextThreadIdOffset",
        "()J",
        |ctx, _args| {
            // JDK ThreadIdentifiers.next() uses Unsafe.getAndAddLong(null,
            // NEXT_TID_OFFSET, 1). Offset 0 routed that RMW into the generic
            // null-base side store with a zero seed, producing illegal tid 0.
            // Use a dedicated, seeded synthetic offset for Java-created
            // threads; VM-built thread mirrors draw from the same counter
            // (`mint_vm_thread_java_tid`), `main` takes `PRIMORDIAL_JAVA_TID`
            // (wave 82). The counter is this VM's own (gc-common w8-a).
            Ok(Some(Value::Long(
                thread_next_tid_offset(ctx.vm_identity()) as i64
            )))
        },
        NativeKind::Bridge,
    );
    // KEEP (the `BootLoader.loadLibrary` no-op): CratonVM has no JNI library
    // to load for any boot-loader library name — it implements the Java-visible
    // natives of `net`/`nio`/`zip` itself — so the load has nothing to do, and
    // short-circuiting it also avoids the JDK's native-library lock.
    // C11: `NativeLibraries.findBuiltinLib(name)` — HotSpot returns the
    // absolute path of a library statically linked into libjvm (e.g. "zip",
    // "net"). We don't statically link any native libraries, so return null,
    // matching the JDK's "not a built-in" contract. Without this, the JDK's
    // NativeLibraries bootstrap throws UnsatisfiedLinkError during
    // Inflater.<clinit>, which cascades into downstream Unsafe panics.
    // Linux real-JDK boot classes such as `java.net.NetworkInterface` call
    // `BootLoader.loadLibrary("net")` during <clinit>. CratonVM implements the
    // Java-visible networking natives itself, and the lower-level
    // `NativeLibraries.load` fallback is already non-fatal, but the JDK
    // bytecode can block indefinitely before reaching it while acquiring the
    // native-library lock. Short-circuit the boot-loader entry point directly.
    registry.register(
        "jdk/internal/loader/BootLoader",
        "loadLibrary",
        "(Ljava/lang/String;)V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP: correct constant. `findBuiltinLib` reports the absolute path of a
    // library STATICALLY LINKED into libjvm; CratonVM links none, and `null`
    // is the JDK's own "not a built-in" answer, which `NativeLibraries.load`
    // handles on its ordinary path.
    registry.register(
        "jdk/internal/loader/NativeLibraries",
        "findBuiltinLib",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
        NativeKind::Bridge,
    );
    // `NativeLibraries.load(NativeLibraryImpl impl, String name, boolean isBuiltin,
    // boolean throwExceptionIfFail)` — the classic JNI native-library loader behind
    // `System.loadLibrary`/`ClassLoader.loadLibrary`. Not previously registered
    // anywhere: on Windows the classes exercised so far apparently resolve via a
    // different bootstrap route, but on Linux real-JDK static init (e.g.
    // Inflater/zip, or JUnit's own bootstrap) calls this directly and an
    // unregistered ACC_NATIVE method throws `UnsatisfiedLinkError` at class-init
    // time, before any test body runs — a hard, platform-specific blocker.
    //
    // Unlike `RawNativeLibraries.load0` (panama.rs) — which backs FFM downcalls
    // into genuine third-party libraries CratonVM does NOT reimplement, e.g.
    // Tomcat's openssl_h needing the real libssl/libcrypto — the libraries this
    // classic path loads are the JDK's OWN internals (zip, net, nio,
    // management, ...), whose Java-visible behavior CratonVM already
    // reimplements as native methods intercepted directly by this registry.
    // A real `dlopen` here is actively counterproductive: the JDK's own
    // `libzip.so` etc. are built to run inside HotSpot's `libjvm.so` process
    // and fail to load standalone (`libjvm.so: cannot open shared object
    // file`) since CratonVM is not that architecture. Try a real load first
    // (some names may resolve, e.g. genuine third-party JNI libs on the
    // classpath), then fall back to the SAME allowlist `System.loadLibrary`
    // uses — and, outside it, report the failure the JDK specifies.
    //
    // WAS a fabricated success: this body computed a handle, dropped the error,
    // and returned `Int(1)` UNCONDITIONALLY. `System.loadLibrary` was fixed to
    // throw `UnsatisfiedLinkError` for names HotSpot cannot load (`sunec`,
    // `jvm`, and `jsig` on Windows), but that fix governs only the intercepted
    // `java/lang/System` native. Any caller reaching library loading through
    // real `java.lang.ClassLoader` / `jdk.internal.loader.NativeLibraries`
    // bytecode instead landed here and still got "loaded" for every name in the
    // universe — the same divergence by another road.
    //
    // MEASURED CONTRACT (JDK 25.0.3, `javap -p -c` plus `lib/src.zip`):
    //
    //   private static native boolean load(NativeLibraryImpl impl, String name,
    //                                      boolean isBuiltin,
    //                                      boolean throwExceptionIfFail);
    //   "Return true if the given library is successfully loaded. If the given
    //    library cannot be loaded for any reason, if throwExceptionIfFail is
    //    false, then this method returns false; otherwise, UnsatisfiedLinkError
    //    will be thrown."
    //
    //   * The ONLY caller is `NativeLibraryImpl.open()`, which returns the
    //     boolean straight through. `NativeLibraries.loadLibrary(Class, String,
    //     boolean)` maps `false` to a null `NativeLibrary`, and `findFromPaths`
    //     then simply tries the NEXT directory of `sun.boot.library.path` /
    //     `java.library.path`. So `false` means "not in this directory", not
    //     "broken" — getting `false` and the throw backwards would turn a
    //     silent wrong answer into a spurious crash.
    //   * `throwExceptionIfFail` is `NativeLibraryImpl.throwExceptionIfFail()` =
    //     `loadLibraryOnlyIfPresent || new File(name).exists()`, and
    //     `ClassLoaderHelper.loadLibraryOnlyIfPresent()` returns `true` on every
    //     platform except macOS. So in practice the argument arrives `true` and
    //     a failure is a THROW. It is still read from the argument, never
    //     assumed: on macOS the `false` return is the live path.
    //   * `name` is NOT a bare library name. `loadLibrary(Class, File)` passes
    //     `file.getCanonicalPath()`, so this native sees an absolute path such
    //     as `<java.home>\bin\zip.dll` or `<java.home>/lib/libzip.so`. Policy
    //     questions are asked about the bare name, so `bare_native_library_name`
    //     decodes it back out.
    //   * The "already loaded in another classloader" `UnsatisfiedLinkError` is
    //     raised by `NativeLibraries.loadLibrary` BYTECODE (the static
    //     `loadedLibraryNames` set) before this native is reached, so it is not
    //     this body's job and must not be duplicated here.
    registry.register(
        "jdk/internal/loader/NativeLibraries",
        "load",
        "(Ljdk/internal/loader/NativeLibraries$NativeLibraryImpl;Ljava/lang/String;ZZ)Z",
        |ctx, args| {
            let impl_obj = match args.first() {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let throw_if_fail = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);
            crate::security_manager::check_host_native_access_or_throw(ctx, &name)?;
            // 1. A genuine load of a genuine library that really is on the
            //    path — a third-party JNI `.so`/`.dll` shipped beside an app.
            //    This is the only branch that yields a usable handle for
            //    `NativeLibrary.findEntry0`, so it is tried first.
            let load_err = match ctx.load_native_library(&name) {
                Ok(lib_index) => {
                    let from_class = impl_obj.and_then(|o| match ctx.get_field_by_name(o, "fromClass") {
                        Value::Object(Some(c)) => Some(c),
                        _ => None,
                    });
                    let loader_id = crate::lang_system::jni_library_loader_id(ctx, from_class);
                    ctx.set_native_library_loader(lib_index, Some(loader_id));
                    if let Some(impl_obj) = impl_obj {
                        // +1 offset, the encoding `findEntry0`/`unload` (panama.rs
                        // and below) decode with `- 1`; 0 is the JDK's own
                        // "not loaded" sentinel, which `open()` asserts on entry.
                        ctx.set_field_by_name(impl_obj, "handle", Value::Long(lib_index + 1));
                    }
                    return Ok(Some(Value::Int(1)));
                }
                Err(err) => err,
            };
            // 2. A JDK-image library whose ENTIRE Java-visible native surface
            //    this VM supplies from Rust. These never `dlopen` here — they
            //    are linked against `libjvm`/`jvm.dll`, which this process is
            //    not — yet a COLD `loadLibrary` of them succeeds on HotSpot, so
            //    reporting failure would be a fresh divergence in the opposite
            //    direction. Same allowlist as `System.loadLibrary`
            //    (`lang_system::is_vm_provided_jdk_library`), deliberately not a
            //    second copy: two lists would drift.
            //
            //    `zip` is added on THIS path only. It is off the
            //    `System.loadLibrary` list because of the dynamic
            //    "already loaded by the boot loader" rule, and on this path that
            //    rule is enforced above us by the JDK's own `loadedLibraryNames`
            //    bytecode — so the question left for this native is the cold
            //    one, whose measured answer is LOADS.
            //
            //    Nothing was opened, so the handle is
            //    `VM_PROVIDED_LIBRARY_HANDLE` (see below): `findEntry0` decodes
            //    it to no table index and takes the whole-process lookup --
            //    every entry point these libraries exist to provide is already
            //    registered in this process as a Rust native.
            let bare = bare_native_library_name(&name);
            // SBR w6 SYSTEM: a file the JDK found INSIDE its own image is the
            // third member of this arm. `System.loadLibrary` used to decide
            // this itself (`lang_system::jdk_image_ships_library`: a bare name
            // whose `lib<name>.so` is in `<java.home>/lib`) and answered
            // success for `awt`, `fontmanager`, `jsound`...; retired, the real
            // `System.loadLibrary` -> `Runtime.loadLibrary0` ->
            // `NativeLibraries.loadLibrary` finds that same file and asks THIS
            // native to open it, and a `dlopen` of a JDK library links against
            // `libjvm` and fails here (`java.awt.image.ColorModel.<clinit>`'s
            // `loadLibraries` died on `libawt.so`). HotSpot's answer is
            // "loads", so this is the native reporting the loaded-in-process
            // library rather than a fabricated success: the argument is an
            // ABSOLUTE PATH under `java.home` that exists on disk, which is
            // what `jdk_image_ships_library` tested by name. A file the user
            // named outside the image still fails below.
            let in_jdk_image = ctx
                .get_system_property("java.home")
                .filter(|h| !h.is_empty())
                .map(|h| {
                    let p = std::path::Path::new(&name);
                    p.starts_with(&h) && p.is_file()
                })
                .unwrap_or(false);
            if crate::lang_system::is_vm_provided_jdk_library(&bare)
                || bare == "zip"
                || in_jdk_image
            {
                // SBR w5 FJP2: a NONZERO handle, as HotSpot's is. The JDK treats
                // 0 as "not loaded": once `Runtime.loadLibrary0` runs the real
                // `NativeLibraries.loadLibrary`, a loader that auto-unloads
                // builds `new Unloader(name, handle, ..)`, which throws
                // `IllegalArgumentException("Invalid handle for native library")`
                // on 0 (measured: `SbrW5Fjp2` `rt.loadLibrary net twice`). The
                // value is HotSpot's builtin-library shape, "the process": it
                // decodes (`- 1`) to no table index, so `findEntry0` takes the
                // whole-process lookup that handle 0 already took, and
                // `unload` refuses it.
                if let Some(impl_obj) = impl_obj {
                    ctx.set_field_by_name(
                        impl_obj,
                        "handle",
                        Value::Long(VM_PROVIDED_LIBRARY_HANDLE),
                    );
                }
                return Ok(Some(Value::Int(1)));
            }
            // 3. Everything else: this VM has no implementation of the library
            //    and no shared object was opened. Report it, in the shape the
            //    argument asks for. Callers like Netty's `NativeLibraryLoader`
            //    and Tomcat's `AprLifecycleListener` are WRITTEN to catch this
            //    and take a pure-Java fallback; the old fabricated success left
            //    them believing a native backend was armed.
            //
            //    REVERT KNOB. This is a behaviour change for every such caller
            //    in every suite, so it gets one switch back to the old answer:
            //    `CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK=1` restores the
            //    unconditional success. It is a bisect aid, not a fix — if a
            //    suite needs it, the real repair is one line in
            //    `lang_system::is_vm_provided_jdk_library`, which is where the
            //    "this VM really does supply that library's natives" claim
            //    belongs and where `System.loadLibrary` reads it too.
            if cratonvm_types::flags::runtime_var("CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK").as_deref()
                == Ok("1")
            {
                return Ok(Some(Value::Int(1)));
            }
            if matches!(
                load_err,
                MethodCallFailed::ExceptionThrown(_)
                    | MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(RuntimeError::UnsatisfiedLinkError { .. }))
            ) {
                return Err(load_err);
            }
            if throw_if_fail {
                return Err(RuntimeError::UnsatisfiedLinkError {
                    message: format!("Can't load library: {name}"),
                }
                .into());
            }
            Ok(Some(Value::Int(0)))
        },
        NativeKind::Bridge,
    );
    // `NativeLibraries.unload(String name, boolean isBuiltin, long handle)` —
    // the companion of `load`, reached from `NativeLibraryImpl.close()` through
    // the `Unloader` that `loadLibrary` registers with the Cleaner for every
    // library loaded by a NON-system class loader.
    //
    // It was registered nowhere. An ACC_NATIVE method with no implementation
    // throws `UnsatisfiedLinkError`, and this one runs on a Cleaner thread where
    // that throw is swallowed — so the library was silently never released. That
    // was harmless only because `load` never really opened anything; now that
    // branch 1 above can report a REAL load with a real handle, the release path
    // is live and has to work.
    //
    // Logical unload, exactly like `RawNativeLibraries.unload0` (panama.rs): the
    // VM's library table is an append-only `Vec` whose index IS the handle, so
    // the entry is tombstoned rather than unmapped — staying mapped is the safe
    // direction, since cached `findEntry0` addresses would otherwise dangle.
    // Real `unload` returns void and reports nothing, so a refusal is ignored.
    registry.register(
        "jdk/internal/loader/NativeLibraries",
        "unload",
        "(Ljava/lang/String;ZJ)V",
        |ctx, args| {
            let handle = match args.get(2) {
                Some(Value::Long(n)) => *n,
                Some(Value::Int(n)) => *n as i64,
                _ => 0,
            };
            // Undo `load`'s +1. Handle 0 — the "allowlisted, but nothing was
            // really opened" case — decodes to -1, which is refused.
            let _unloaded = ctx.unload_native_library(handle - 1);
            Ok(None)
        },
        NativeKind::Bridge,
    );
    // KEEP: genuine no-op here. The native's only effect in HotSpot is to
    // hand the boot loader's unnamed Module to the VM's own module table;
    // CratonVM resolves modules through the class manager, never through
    // that table, so there is no state to record.
    registry.register(
        "jdk/internal/loader/BootLoader",
        "setBootLoaderUnnamedModule0",
        "(Ljava/lang/Module;)V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "setCurrentThread",
        "(Ljava/lang/Thread;)V",
        |ctx, args| {
            // `carrier.setCurrentThread(t)`: what `Thread.currentThread()`
            // answers on this thread from now on -- a carrier mounting a
            // virtual thread (`VirtualThread.mount`), unmounting it, or one
            // switching to its carrier's identity for a moment
            // (`switchToCarrierThread`). HotSpot's `JVM_SetCurrentThread`
            // re-points the `JavaThread`'s `vthread`. Served by the VM
            // (`NativeContext::set_current_thread_object`): only the JDK's own
            // continuation machinery calls it, and only when the JDK drives
            // virtual threads (the VM's `jdk_continuation` module); a bound
            // virtual thread never reaches it.
            if let Some(Value::Object(Some(t))) = args.get(1) {
                ctx.set_current_thread_object(*t);            }
            Ok(None)
        },
        NativeKind::Bridge,
    );
    // KEEP: correct constant. `null` is the JDK's "no scoped-value bindings
    // on this stack" answer; `ScopedValue` treats it as the empty snapshot.
    // CratonVM never installs bindings (the `setScopedValueCache` companion
    // below is likewise inert), so there are genuinely none to report.
    registry.register(
        "java/lang/Thread",
        "findScopedValueBindings",
        "()Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
        NativeKind::Bridge,
    );
    // The per-thread scoped-value cache. `ScopedValue$Cache.put` stores into the array
    // it installed with `setScopedValueCache` and re-reads it with `scopedValueCache`
    // (`setKeyAndObjectAt`), so a cache that always reads back null is not a miss: the
    // store dereferences null and every `ScopedValue.get` of a bound value threw. The
    // array is kept per Java thread (by `Thread.tid`) behind a global root.
    registry.register(
        "java/lang/Thread",
        "scopedValueCache",
        "()[Ljava/lang/Object;",
        |ctx, _args| {
            let thread = ctx.current_thread_object();
            let tid = match ctx.get_field_by_name(thread, "tid") {
                Value::Long(t) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let vm = ctx.vm_identity();
            let handle = SCOPED_VALUE_CACHES.with(vm, |m| m.get(&tid).copied());
            let cache = handle.and_then(|h| ctx.resolve_global_root(h));
            Ok(Some(Value::Object(cache)))
        },
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "setScopedValueCache",
        "([Ljava/lang/Object;)V",
        |ctx, args| {
            let thread = ctx.current_thread_object();
            let tid = match ctx.get_field_by_name(thread, "tid") {
                Value::Long(t) => t,
                _ => return Ok(None),
            };
            let new_handle = match args.first() {
                Some(Value::Object(Some(arr))) => ctx.add_global_root(*arr),
                _ => 0,
            };
            let vm = ctx.vm_identity();
            let old = SCOPED_VALUE_CACHES.with(vm, |m| {
                if new_handle == 0 {
                    m.remove(&tid)
                } else {
                    m.insert(tid, new_handle)
                }
            });
            if let Some(old) = old {
                ctx.remove_global_root(old);            }
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "ensureMaterializedForStackWalk",
        "(Ljava/lang/Object;)V",
        |_ctx, _args| {
            // KEEP: genuine no-op. This is a HotSpot escape-analysis hint —
            // it forces a scalar-replaced local back onto the real stack so a
            // StackWalker can see it. CratonVM's stack walker reads live
            // interpreter/JIT frames directly and the JIT never scalar-replaces
            // an object out of a frame it then has to expose, so there is
            // nothing to materialise.
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "getStackTrace0",
        "()Ljava/lang/Object;",
        native_thread_get_stack_trace,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "dumpThreads",
        "([Ljava/lang/Thread;)[[Ljava/lang/StackTraceElement;",
        native_thread_dump_threads,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "getThreads",
        "()[Ljava/lang/Thread;",
        native_thread_get_threads,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Thread",
        "clearInterruptEvent",
        "()V",
        |ctx, _args| {
            // Real static `Thread.interrupted()` clears the Java `interrupted` FIELD
            // and then calls this JDK-internal method. We use it as the clear-side
            // hook to also reset the VM-side interrupt atomic (set by
            // native_thread_interrupt), keeping the field and the atomic — which
            // LockSupport.park / Object.wait / Condition.await poll — in sync. Without
            // this, the atomic would stay set after `interrupted()` cleared the field,
            // making the next park() return spuriously. `is_interrupted(true)` reads
            // and clears the current thread's atomic (the clear is what we want here).
            let _ = ctx.is_interrupted(true);
            Ok(None)
        },
        NativeKind::Bridge,
    );

    // --- java.lang.Throwable (native methods) ---
    registry.register(
        "java/lang/Throwable",
        "fillInStackTrace",
        "(I)Ljava/lang/Throwable;",
        native_throwable_fill_in_stack_trace,
        NativeKind::Bridge,
    );

    // --- java.lang.Float / Double (native bit manipulation) ---
    registry.register(
        "java/lang/Float",
        "floatToRawIntBits",
        "(F)I",
        native_float_to_raw_int_bits,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Float",
        "intBitsToFloat",
        "(I)F",
        native_float_int_bits_to_float,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Double",
        "doubleToRawLongBits",
        "(D)J",
        native_double_to_raw_long_bits,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Double",
        "longBitsToDouble",
        "(J)D",
        native_long_bits_to_double,
        NativeKind::Bridge,
    );

    // --- java.lang.Runtime ---
    registry.register(
        "java/lang/Runtime",
        "availableProcessors",
        "()I",
        native_runtime_available_processors,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime",
        "gc",
        "()V",
        |ctx, _args| {
            // zs s7/jf: `jdk.SystemGC` on the caller (see `lang_system`'s twin).
            crate::hooks::jfr_force_gc_as_system_gc(ctx);
            Ok(None)
        },
        NativeKind::Bridge,
    );
    // W7-86. `Runtime.exit(int)` is an INSTANCE method (`javap -p` on Adoptium
    // 25.0.3.9: `public void exit(int)`), so `args[0]` is the `Runtime`
    // receiver and `args[1]` is the status. `native_system_exit` serves the
    // STATIC `System.exit(int)`, where `args[0]` IS the status — pointing this
    // triple at it made every `Runtime.getRuntime().exit(n)` read the receiver
    // where an `Int` was expected, take the `_ => 0` arm and **exit 0**.
    //
    // Measured on this Windows host: `Runtime.getRuntime().exit(7)` returned
    // exit code 7 on HotSpot 25.0.3.9 and 0 on CratonVM, while `System.exit(7)`
    // returned 7 on both.
    //
    // `native_runtime_exit` is the sibling that already handles the instance
    // shape (`args.get(1)`, falling back to `args.first()`), and
    // `lang_system.rs:1423` already registers it for this exact triple — but
    // this line ran LATER and last-write-wins made the correct one dead. The
    // `--dump-native-registry` census shows both rows, this one with
    // `owns_slot: true`. Compatible mode (`--fast-boot`, default);
    // HotSpot-parity fix.
    registry.register("java/lang/Runtime", "exit", "(I)V", native_runtime_exit, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/Runtime",
        "freeMemory",
        "()J",
        native_runtime_free_memory,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime",
        "totalMemory",
        "()J",
        native_runtime_total_memory,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime",
        "maxMemory",
        "()J",
        native_runtime_max_memory,
        NativeKind::Bridge,
    );

    registry.register(
        "java/lang/Runtime",
        "version",
        "()Ljava/lang/Runtime$Version;",
        crate::lang_system::native_runtime_version, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime$Version",
        "feature",
        "()I",
        crate::lang_system::native_runtime_version_feature, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime$Version",
        "build",
        "()Ljava/util/Optional;",
        crate::lang_system::native_runtime_version_build, cratonvm_native_api::NativeKind::Bridge,
    );
    // Every spawn route funnels into `native-io`'s `spawn_and_wrap`, including
    // the `ProcessBuilder.start` that this crate does NOT own. Hand it the
    // SecurityManager gate here, alongside the exec registrations, so the two
    // cannot drift apart. Idempotent.
    crate::lang_system::install_spawn_policy_hook();

    // Runtime.exec overloads — spawn subprocesses via std::process::Command
    //
    // W7-17 N1 — `SyntheticStub`, STATED, and the tag is the whole point.
    // These six and `java/lang/ProcessBuilder.start` share ONE mint:
    // `native-io/src/process.rs::spawn_and_wrap_with_redirects`, whose
    // `refused_class(ctx, SYNTHETIC_PROCESS_CLASS, PROC_FIELD_COUNT)?`
    // propagates a `--real-jdk` refusal out to the caller as
    // `NoClassDefFoundError: cratonvm/synthetic/Process`. `ProcessBuilder.start`
    // was re-tagged `SyntheticStub` for exactly that reason; these six were left
    // ambient `Bridge`, so the OLDER spawn API stayed open into strict mode.
    // **A retag that pins one caller of a shared mint must enumerate the
    // callers** — measured 2026-08-12, one probe, both halves:
    //
    //     OK   ProcessBuilder.start (the pinned half)
    //     FAIL Runtime.exec(String[]) -> NoClassDefFoundError:
    //                                    cratonvm/synthetic/Process
    //
    // `Runtime.exec` is ordinary bytecode on the image (`acc_native: false,
    // has_code: true`), so by contract §1.4 the real method outranks any bridge:
    // strict drops these, the JDK's own `exec` runs, and it delegates to the
    // real `ProcessBuilder.start()` -> `ProcessImpl.forkAndExec`, which this
    // crate registers. Default `--fast-boot` is deliberately unchanged —
    // `SyntheticStub` survives there and still shadows `exec`.
    registry.register(
        "java/lang/Runtime",
        "exec",
        "(Ljava/lang/String;)Ljava/lang/Process;",
        native_runtime_exec_string,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/Runtime",
        "exec",
        "([Ljava/lang/String;)Ljava/lang/Process;",
        native_runtime_exec_array,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/Runtime",
        "exec",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/Process;",
        native_runtime_exec_string_env,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/Runtime",
        "exec",
        "([Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/Process;",
        native_runtime_exec_array_env,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/Runtime",
        "exec",
        "(Ljava/lang/String;[Ljava/lang/String;Ljava/io/File;)Ljava/lang/Process;",
        native_runtime_exec_string_env_dir,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/Runtime",
        "exec",
        "([Ljava/lang/String;[Ljava/lang/String;Ljava/io/File;)Ljava/lang/Process;",
        native_runtime_exec_array_env_dir,
        NativeKind::SyntheticStub,
    );

    // --- java.lang.Math / StrictMath (native transcendental functions) ---
    //
    // CLOSED 2026-08-12 — W7-54-strictmath-fdlibm-family-RETIRED-20260917.md. This site used to
    // carry a standing VULN/LIMITATION note, and it was accurate:
    // `java.lang.StrictMath` is contractually required to produce bit-for-bit
    // identical results on every platform and every VM — its spec names the
    // fdlibm algorithms, the same code the reference JDK ships — and CratonVM
    // registered the SAME backing for both `Math` and `StrictMath`, delegating
    // the whole transcendental surface to the host libm. Platform libm is not
    // fdlibm-equivalent, so `StrictMath` here violated its own specification.
    //
    // `register_math_natives` now splits the two classes for the eighteen
    // functions where the split is real. This was never theoretical: replaying
    // a HotSpot oracle against the libm we actually link, every one of them
    // disagreed with fdlibm — cbrt 30.98%, cosh 28.55%, sinh 28.08%, pow 9.73%,
    // exp 9.62%, down to atan 0.01% — and the `log` gap was already reaching
    // users through `Random.nextGaussian()`, which returned a different double
    // from HotSpot for every seed
    // (W7-44-numberformat-enum-and-double-tostring.md).
    //
    // Two functions remain shared between the classes, for opposite reasons.
    // `sqrt`, because IEEE 754 requires it correctly rounded, so the hardware
    // instruction IS the fdlibm result — a theorem, not an accident, and the
    // reason its census row is a structural zero. `IEEEremainder`, because its
    // spec fixes the result exactly for BOTH classes ("as prescribed by the
    // IEEE 754 standard"), so both now get the fdlibm body; the old one was
    // wrong for `Math` too, on 49.83% of pairs and by an unbounded amount.
    //
    // `Math` keeps libm everywhere else deliberately. libm satisfies `Math`'s
    // 1-ULP bound, so pointing both classes at the fdlibm bodies would fix
    // nothing and would only make the overwhelmingly more common caller slower.
    lang_math::register_math_natives(registry, "java/lang/Math");
    lang_math::register_math_natives(registry, "java/lang/StrictMath");
    // Test fixtures and real-JDK bytecode in default mode still emit standard
    // autoboxing calls such as Integer.valueOf(int). These are bytecode methods
    // in the JDK, not ACC_NATIVE, but the interpreter's static dispatch probes
    // the native registry for non-object primitive receivers; keep the exact
    // wrapper valueOf/intValue surface available in the essential registry.
    lang_math::register_wrapper_natives(registry);

    // --- sun/misc/Unsafe + jdk/internal/misc/Unsafe ---
    register_unsafe_natives(registry);

    // --- java.lang.reflect.Array ---
    // C40: register the full set (getLength, get/set, primitive-typed
    // get*/set*, newInstance/newArray/multiNewArray) so real-JDK bytecode
    // that calls into `java.lang.reflect.Array` (e.g. Picocli's reflective
    // array handling) links successfully.  Previously only newInstance
    // and getLength were wired in real-JDK mode, leaving
    // `Array.set(Object,int,Object)` and the typed getters/setters
    // unlinked — which tripped UnsatisfiedLinkError on Array.set(...).
    register_reflect_array_natives(registry);

    // --- Signal handling ---
    // `findSignal0` maps a signal NAME to its number. The old constant `0`
    // was wrong twice over: 0 is not a valid signal number, and every name
    // mapped to the SAME number, so `Signal.handle(new Signal("INT"), h)`
    // and `Signal.handle(new Signal("TERM"), h)` collided on one key in
    // `Signal.handlers`/`Signal.signals` — the second registration silently
    // evicted the first. Answer the real POSIX numbers so distinct signals
    // stay distinct, and -1 for a name we do not know, which is exactly what
    // `JVM_FindSignal` returns and what `Signal(String)` turns into the
    // spec'd `IllegalArgumentException("Unknown signal: ...")`.
    registry.register(
        "jdk/internal/misc/Signal",
        "findSignal0",
        "(Ljava/lang/String;)I",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(Value::Int(posix_signal_number(&name))))        },
        NativeKind::Bridge,
    );
    // `handle0(sig, handler)` installs `handler` and returns the PREVIOUS
    // handler (0 = SIG_DFL, 1 = SIG_IGN, -1 = "reserved by the VM/OS", which
    // `Signal.handle` converts into IllegalArgumentException). CratonVM
    // installs no OS signal handlers, so the Java handler that was just
    // registered will never fire. We still report 0 rather than -1: every
    // in-tree caller (`Terminator.setup` during initPhase1, Tomcat/log4j
    // shutdown hooks) treats the exception as fatal-to-that-feature, so
    // throwing here would break boot without buying the caller any working
    // signal delivery. Deliberate silent accept; see the wave-2 stub-removal
    // report.
    //
    // CORRECTED 2026-08-12 (lane C9). This comment used to end "...and the
    // VM's own shutdown path runs through `Runtime.addShutdownHook`, not
    // signals", offering that as the reason the silent accept is harmless.
    // **That premise is false and it is the second-worst thing about it: the
    // comment reads as an assurance that orderly shutdown still works.** It
    // does not. `Runtime.addShutdownHook` is intercepted by a native in
    // `lang_system.rs` that roots the hook in a `SHUTDOWN_HOOKS` vector whose
    // only reader is `removeShutdownHook`, and no exit path in this VM runs
    // it — not `System.exit`, not `Runtime.exit`, not the launcher's
    // post-`main` return, not the uncaught-exception path. So a Tomcat or
    // log4j teardown reaches this VM through neither door: the signal
    // handler is accepted and never fires, AND the shutdown hook is accepted
    // and never runs. The accept above is still the right call for boot; what
    // is removed is the false consolation. See
    // `docs/feature-designs/native-dispatch-architecture.md`, and do
    // not restore the old sentence until its `RShutdownHooks` vector is green.
    //
    // SBR wave 5 FJP2: the accept stays, but the answer is now the PREVIOUS
    // disposition from a per-VM table, as `JVM_RegisterSignal` answers it (a
    // re-registration returns `2` and the JDK hands back the handler it
    // displaced; it used to be told SIG_DFL), and the VM's own signals are
    // refused with `-1` as HotSpot refuses them. See
    // `deprecated_internal::native_signal_handle0`.
    registry.register(
        "jdk/internal/misc/Signal",
        "handle0",
        "(IJ)J",
        crate::deprecated_internal::native_signal_handle0,
        NativeKind::Bridge,
    );

    // --- I/O essential natives ---
    registry.register("java/io/File", "<clinit>", "()V", native_file_clinit, cratonvm_native_api::NativeKind::Bridge);
    // The three `registerNatives` below are JNI symbol binding only — see
    // java/lang/Object.registerNatives.
    registry.register(
        "java/io/FileInputStream",
        "registerNatives",
        "()V",
        native_noop, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileOutputStream",
        "registerNatives",
        "()V",
        native_noop, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileDescriptor",
        "registerNatives",
        "()V",
        native_noop, cratonvm_native_api::NativeKind::Bridge,
    );
    // FileDescriptor.getHandle(I)J — Windows-only: returns the OS handle for
    // the given fd number.  0..=2 map to the standard-stream pseudo-handles
    // (we don't surface the real Windows HANDLE, and <clinit> accepts -1 for
    // non-standard fds).
    registry.register(
        "java/io/FileDescriptor",
        "getHandle",
        "(I)J",
        |_ctx, args| {
            let fd = match args.get(0) {
                Some(Value::Int(v)) => *v,
                Some(Value::Long(v)) => *v as i32,
                _ => -1,
            };
            // Round nio20 wave 3, lane L5 (proposal NIO20W2-L4-3): only
            // Windows has a `handle`. HotSpot's Unix `getHandle(int)` is -1
            // for every fd, so `FileDescriptor.in/out/err.handle` read -1
            // there (reflection-visible); answering 0/1/2 also made stdin's
            // `handle == 0` indistinguishable from an unpopulated object.
            // Every descriptor reader still finds 0..=2 through `fd` (the
            // fd-first readers directly, `nio_native::descriptor_fd_id`
            // through its `handle == -1` rule).
            //
            // JIT r23 wave 2, lane L6: on Windows HotSpot answers the real
            // `GetStdHandle` value (`CRATONVM_FD_FIELDS_PER_PLATFORM`; `=0`
            // keeps the fd number). `descriptor_fd_id` recognises that exact
            // pair as the stream; `FileDescriptor.sync0` asks the same.
            let handle = if cfg!(windows) && (0..=2).contains(&fd) {
                cratonvm_native_io::nio_native::standard_stream_handle(
                    fd,
                    cratonvm_native_io::nio_native::fd_fields_per_platform_enabled(),
                )
            } else {
                -1i64
            };
            Ok(Some(Value::Long(handle)))
        },
        NativeKind::Bridge,
    );
    // W4: was a hard `false` on the (incorrect) grounds that CratonVM never
    // opens an O_APPEND descriptor — `FileDescriptorTable::open_write` passes
    // `.append(true)` straight to the OS. Ask the host for the three standard
    // descriptors, which are the only `fd` values that are real OS descriptors
    // here (same 0..=2 range the `getHandle` native above maps through) and
    // the only ones the JDK's sole caller, `FileDescriptor(int)`, asks about.
    // See `native_file_descriptor_get_append` for the escalation covering the
    // remaining, table-id-only descriptors.
    registry.register(
        "java/io/FileDescriptor",
        "getAppend",
        "(I)Z",
        native_file_descriptor_get_append,
        NativeKind::Bridge,
    );
    // FileDescriptor.sync0()V — fsync. `this` carries the fd id that `open0`
    // stored in the Windows `handle` long / POSIX `fd` int slot.
    //
    // The durability loss a no-op here actually causes is the *userspace*
    // buffer: our writable fds sit behind a `BufWriter`, so bytes a caller
    // believes are on disk (Lucene `FSDirectory.sync`, an H2 log commit) may
    // still be in VM memory. Push those to the OS first, then ask for a real
    // `sync_all`. `rw_sync` only accepts random-access entries, so the
    // buffered-stream case stops at the flush — errors are deliberately
    // swallowed, matching `FileDescriptor.sync`'s contract of throwing only
    // `SyncFailedException` (which we have no way to raise from a path that
    // never had a durable handle to begin with).
    registry.register(
        "java/io/FileDescriptor",
        "sync0",
        "()V",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(None);
            };
            // native-io's one descriptor reader (JIT r23 w2 L6, proposal 4 of
            // `jit-r23-w1-L6-proposals.md`), keeping this body's own rule that
            // a standard stream (0..=2, including a Windows stream's real
            // `HANDLE`) has no table entry to sync.
            let fd = cratonvm_native_io::nio_native::descriptor_fd_id(&*ctx, this)
                .filter(|id| *id > 2);
            // NO DESCRIPTOR AT ALL is the one case that must not be quiet.
            // `new FileDescriptor().sync()` — a descriptor that was never opened —
            // is `SyncFailedException: sync failed` on HotSpot, and answering
            // normally told a caller its data was durable when nothing had been
            // written anywhere. MEASURED.
            //
            // A descriptor that IS open but whose entry `rw_sync` declines (the
            // buffered-stream case the comment above describes) keeps the
            // deliberate swallow: there the flush did happen and only the fsync is
            // unavailable, which is a host limitation rather than a lie about a
            // handle that does not exist.
            //
            // Round 15 wave 4, lane jdkonly4
            // (`r15w3-jdkonly3-residual-new-object-init-builders-patch-FIXED-20260930.md`
            // #8): `throwable_mint::construct_thrown` -- a throwing constructor
            // propagates rather than being ignored with the half-built object
            // thrown; the message is a fresh string, as libjava's
            // `JNU_ThrowByName` makes it.
            let Some(fd) = fd else {
                let msg = ctx.create_string_uninterned("sync failed");
                return Err(crate::throwable_mint::construct_thrown(
                    ctx,
                    "java/io/SyncFailedException",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(msg))],
                    || {
                        cratonvm_types::error::RuntimeError::IOException {
                            message: "sync failed".into(),
                        }
                        .into()
                    },
                ));
            };
            let _ = ctx.fd_table().flush(fd);
            let _ = ctx.fd_table().rw_sync(fd, false);
            Ok(None)
        },
        NativeKind::Bridge,
    );

    // --- jdk/internal/misc/VM (needed by IntegerCache, LongCache, etc.) ---
    // `VM.initialize()` is libjava's one-shot hook for caching the JVM handle
    // it later makes `JVM_*` calls through. CratonVM dispatches those entry
    // points from this registry, and the JVM's own bootstrap state machine is
    // driven separately by `set_init_level` (see `initLevel` right below), so
    // there is no handle to cache and no state to flip here.
    registry.register(
        "jdk/internal/misc/VM",
        "initialize",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    // WP1.3: VM.initLevel() and VM.awaitInitLevel(int) track the JVM
    // bootstrap state machine (mirroring HotSpot's `VM::_init_level`):
    //   * 1 — after `Vm::new` finishes (primordial classes loaded),
    //   * 2 — after `System.initPhase1()` completes,
    //   * 3 — after `System.initPhase2()` completes,
    //   * 4 — right before `main()` runs.
    // The vm-cli bootstrap path drives transitions via
    // `cratonvm_native_api::init_level::set_init_level`.
    // Level <2 routes `ClassLoader.getSystemClassLoader()` through
    // `getBuiltinAppClassLoader()` which reads `ClassLoaders.APP_LOADER`
    // (populated by `ClassLoaders.<clinit>`).  Levels 3/4 route through
    // `System.scl` — populated only when we run initPhase3, so on an
    // initphase3-less real-JDK run the counter stops at 2 and keeps
    // the pre-initPhase3 code path viable.
    // Minimum floor of 2 on the synthetic JDK: its class initialisers assume
    // at least initPhase1 completed, and a live level of 0 or 1 made them
    // throw `IllegalStateException("Not yet initialized")`. Over a real JDK
    // the callback answers the JDK's `initLevel` static, as the JDK body and
    // HotSpot do (j27 lane FBTAIL; `phases_early::vm_init_level_native`, the
    // one callback both registrations use). Under `--compatible` over a real
    // JDK the registry drops this and the `isBooted` registration below
    // (the `jdk-vm-init-level` HotSpot-alignment row, default ON, j27 lane
    // VMALIGN; `retired_shadow::is_vm_init_level_row`), so the JDK bodies
    // run; these callbacks serve `-jdk-vm-init-level` and the synthetic JDK.
    let init_level = vm_init_level_native(registry);
    registry.register("jdk/internal/misc/VM", "initLevel", "()I", init_level, cratonvm_native_api::NativeKind::Bridge);
    // isBooted() -> boolean. The real `jdk.internal.misc.VM.isBooted()` is
    // `initLevel >= SYSTEM_BOOTED (4)` over the static; CratonVM drives boot
    // natively and, before j27 lane FBLEVEL, never raised that static to 4 on
    // the fast boot, so the real bytecode returned false there. (It now does;
    // see the `jdk-vm-init-level` row above.) Security-sensitive JDK paths gate on it — e.g.
    // `sun.security.util.ResourcesMgr.getBundle` throws
    // `InternalError("Expected to use ResourceBundle only after booted")`,
    // breaking `Subject.getPrivateCredentials` (JASPIC auth) and any other
    // post-boot ResourceBundle load. By the time app/test code runs the VM is
    // booted, so return 1 — consistent with `sun/misc/VM.isBooted`.
    registry.register("jdk/internal/misc/VM", "isBooted", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // The real JDK predicates read VM.classFileMajorVersion, a private static
    // normally initialized by HotSpot during early boot. CratonVM does not run
    // that path, so the field remains zero and JDK module-info parsing rejects
    // every descriptor as "Unsupported major.minor version 53.0" before
    // Elasticsearch's embedded provider modules can load. Mirror JDK 21+'s
    // predicate semantics but use the classfile ceiling this VM advertises via
    // java.class.version / multi-release handling: Java 25 => major 69.
    registry.register(
        "jdk/internal/misc/VM",
        "isSupportedClassFileVersion",
        "(II)Z",
        |_ctx, args| {
            let major = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let minor = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            // A PREVIEW minor (65535) is legal only for the CURRENT major.
            // JDK 25's own predicate:
            //
            //     if (major < 45 || major > classFileMajorVersion) return false;
            //     if (major <= 55) return true;   // any minor
            //     return minor == 0
            //         || (minor == 65535 && major == classFileMajorVersion);
            //
            // Accepting `(68, 65535)` -- a JDK 24 class file claiming preview
            // -- is the shape this gate exists to refuse: preview features are
            // not forward-compatible, and a class file that says "preview" for
            // an older release is one whose bytecode this VM has no business
            // running. MEASURED by `JdkInternalSweep.java`.
            const CURRENT_MAJOR: i32 = 69;
            let ok = (45..=CURRENT_MAJOR).contains(&major)
                && (major < 56 || minor == 0 || (minor == 65535 && major == CURRENT_MAJOR));
            Ok(Some(Value::Int(ok as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/VM",
        "isSupportedModuleDescriptorVersion",
        "(II)Z",
        |_ctx, args| {
            let major = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let minor = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let ok = (53..=69).contains(&major) && (major < 56 || minor == 0 || minor == 65535);
            Ok(Some(Value::Int(ok as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // WP1.3: awaitInitLevel(int) blocks on the process-wide condvar
    // until the level reaches `n`.  Matches HotSpot's
    // `JVM_AwaitInitLevel` — used by JDK internals (e.g. the
    // `InnocuousThread` factory) to gate startup work until the
    // boot classloader is ready.  Java signature is
    // `void awaitInitLevel(int) throws InterruptedException`; we
    // don't inject interrupts here — threads either exit normally
    // on wake or block indefinitely if the level never reaches the
    // target (which matches HotSpot when no interrupt is pending).
    registry.register(
        "jdk/internal/misc/VM",
        "awaitInitLevel",
        "(I)V",
        |_ctx, args| {
            let target = match args.first() {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            cratonvm_native_api::init_level::await_init_level(target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/VM",
        "getSavedProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        lang_system::native_vm_get_saved_property, cratonvm_native_api::NativeKind::Bridge,
    );
    // The setter half of the field above. See `native_vm_save_properties`'s
    // own doc comment: real `System.initPhase1()` is left as real bytecode on
    // a real-JDK-backed image, and its own first substantive call is to this
    // method, whose real precondition (`initLevel() == 0`) this VM's
    // `VM.initLevel()` native could never satisfy while it floor-clamped to 2
    // (only the synthetic JDK keeps that floor since j27; see the doc).
    // Never registered before now.
    registry.register(
        "jdk/internal/misc/VM",
        "saveProperties",
        "(Ljava/util/Map;)V",
        lang_system::native_vm_save_properties, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- jdk/internal/constant/ConstantUtils ---
    // ConstantUtils.concat(String, Object, String) uses JLA.concat() which
    // requires SharedSecrets.getJavaLangAccess() to be populated (happens in
    // System.initPhase1).  Since we don't run initPhase1 fully, JLA is null
    // and concat() NPEs.  Provide a native that does the concatenation directly.
    registry.register(
        "jdk/internal/constant/ConstantUtils",
        "concat",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let prefix = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let middle = match args.get(1) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                Some(Value::Object(None)) => "null".to_string(),
                _ => String::new(),
            };
            let suffix = match args.get(2) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let result = format!("{}{}{}", prefix, middle, suffix);
            let s = ctx.create_string(&result);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- jdk/internal/misc/CDS (needed by IntegerCache, StringUTF16, etc.) ---
    // Class Data Sharing is an OPTIONAL HotSpot feature, and the JDK's own
    // callers are written for a JVM that does not have it: with sharing
    // disabled (`isSharingEnabled()` → false, below) the spec-correct
    // behaviour of `initializeFromArchive` / `dumpClassList` /
    // `dumpDynamicArchive` / `logLambdaFormInvoker` is precisely to do
    // nothing. These are no-ops in HotSpot too whenever no archive is mapped.
    registry.register(
        "jdk/internal/misc/CDS",
        "initializeFromArchive",
        "(Ljava/lang/Class;)V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/CDS",
        "isSharingEnabled",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/CDS",
        "isDumpingClassList0",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/CDS",
        "isDumpingArchive0",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/CDS",
        "logLambdaFormInvoker",
        "(Ljava/lang/String;)V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/CDS",
        "getCDSConfigStatus",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(0))),
        NativeKind::Bridge,
    );
    // T15: JDK 25 CDS adds a seed native for deterministic archive dumping.
    registry.register(
        "jdk/internal/misc/CDS",
        "getRandomSeedForDumping",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(0))),
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/CDS",
        "dumpClassList",
        "(Ljava/lang/String;)V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/misc/CDS",
        "dumpDynamicArchive",
        "(Ljava/lang/String;)V",
        native_noop,
        NativeKind::Bridge,
    );

    // --- jdk/internal/misc/PreviewFeatures ---
    // <clinit> calls this once to cache the `ENABLED` constant. It's reached
    // any time `Class.isUnnamedClass()` is used (e.g. JUnit's launcher during
    // discovery), which otherwise aborts every real-JDK run with
    // UnsatisfiedLinkError before a single test executes.
    //
    // This answers the SAME bit as the class manager's preview gate (the VM's
    // `VmConfig::enable_preview`, `NativeSystemAccess::preview_enabled`),
    // because on HotSpot it is the same bit: measured on Adoptium 25.0.3.9,
    // `PreviewFeatures.isEnabled` is false on plain `java` and true under
    // `--enable-preview`. Answering 0 while the VM had accepted a `69.65535`
    // class file — or the reverse — would be a divergence manufactured by the
    // fix that added the gate. Per VM since interpreter round i5 wave 101, lane
    // L5 (it read a process global the launcher set).
    registry.register(
        "jdk/internal/misc/PreviewFeatures",
        "isPreviewEnabled",
        "()Z",
        |ctx, _args| Ok(Some(Value::Int(i32::from(ctx.preview_enabled())))),
        NativeKind::Bridge,
    );

    // jdk/internal/misc/ScopedMemoryAccess — JEP 471 panama foreign memory.
    // registerNatives is called from <clinit>. We don't support scoped memory
    // natives yet (they're used by Vector API and Foreign Memory API), but
    // noop'ing registerNatives keeps the class loadable.
    registry.register(
        "jdk/internal/misc/ScopedMemoryAccess",
        "registerNatives",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );

    // --- jdk/internal/perf/Perf (C27) ---
    // JDK performance-counter infrastructure. Perf.getPerf().createLong(...) is
    // invoked during early bootstrap (e.g., by the JDK's internal timing hooks)
    // and by JBoss Modules for its own counters. We don't track perf data; we
    // hand out benign defaults — empty/zeroed direct ByteBuffers for the
    // create* methods, zero/no-op for the rest. See lang_system::native_perf_*.
    registry.register(
        "jdk/internal/perf/Perf",
        "registerNatives",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/perf/Perf",
        "attach0",
        "(I)Ljava/nio/ByteBuffer;",
        lang_system::native_perf_attach0,
        NativeKind::Bridge,
    );
    // Perf.detach(ByteBuffer) releases the mapping that `attach` handed out.
    // Our `create*` return ordinary direct ByteBuffers
    // (lang_system::native_perf_*), so there is nothing to unmap. Since zt
    // t9/pl `attach0(0)` returns a buffer over this VM's own perf-data view,
    // which, as HotSpot's own perf memory, stays mapped until the process
    // ends (`PerfDataFile::attach_view`): detaching it is a no-op on HotSpot
    // too. Since zt t10/pl2 `attach0(<other pid>)` maps that VM's perf data
    // read-only; `Perf.attach`'s cleaner calls this with that buffer, which
    // unmaps it (`lang_system::native_perf_detach`; anything else is left).
    registry.register(
        "jdk/internal/perf/Perf",
        "detach",
        "(Ljava/nio/ByteBuffer;)V",
        lang_system::native_perf_detach,
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/perf/Perf",
        "createLong",
        "(Ljava/lang/String;IIJ)Ljava/nio/ByteBuffer;",
        lang_system::native_perf_create_long,
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/perf/Perf",
        "createByteArray",
        "(Ljava/lang/String;II[BI)Ljava/nio/ByteBuffer;",
        lang_system::native_perf_create_byte_array,
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/perf/Perf",
        "highResCounter",
        "()J",
        lang_system::native_perf_high_res_counter,
        NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/perf/Perf",
        "highResFrequency",
        "()J",
        lang_system::native_perf_high_res_frequency,
        NativeKind::Bridge,
    );

    // Win32-side filesystem init — noop in our VM (path canonicalization
    // and FS flags are built in Rust, not JNI).
    registry.register(
        "java/io/WinNTFileSystem",
        "initIDs",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/UnixFileSystem",
        "initIDs",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    // Guard real-JDK File.<clinit> against missing System property entries.
    // JDK constructors call:
    //   props.getProperty("file.separator").charAt(0)
    //   props.getProperty("path.separator").charAt(0)
    // and can NPE before our Java-side bootstrap finishes wiring props.
    // Seed the key fields directly in the FS object.
    registry.register(
        "java/io/WinNTFileSystem",
        "<init>",
        "()V",
        native_platform_filesystem_init, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/UnixFileSystem",
        "<init>",
        "()V",
        native_platform_filesystem_init, cratonvm_native_api::NativeKind::Bridge,
    );

    // T15: java.lang.reflect.Field accessors — in real-JDK mode the JDK
    // bytecode `Field.getName()` reads a `name` field at the real-JDK
    // layout offset, but our `create_field_object` populates a synthetic
    // layout instead.  Register native overrides that honour OUR layout
    // (field 0 = declaring class, 1 = name, 2 = type, 3 = modifiers,
    // 4 = slot, 5 = descriptor) so the reflection path works regardless
    // of mode.
    registry.register(
        "java/lang/reflect/Field",
        "getName",
        "()Ljava/lang/String;",
        lang_class::native_field_get_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getType",
        "()Ljava/lang/Class;",
        lang_class::native_field_get_type, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getModifiers",
        "()I",
        lang_class::native_field_get_modifiers, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        lang_class::native_field_get_declaring_class, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setAccessible",
        "(Z)V",
        lang_class::native_field_set_accessible, cratonvm_native_api::NativeKind::Bridge,
    );
    // The `Field` value doors. `Bridge`, stated, in every configuration but
    // one: under `--real-jdk` with `CRATONVM_FIELD_DOORS_INTRINSIC`, a reviewed
    // `Intrinsic`, so `resolve_dispatch` step 2 serves them with these natives
    // instead of the JDK's `Field` bytecode, as `--compatible` does
    // (`NativeMethodRegistry::jdk_only_field_doors`; interpreter round i3
    // wave 73, lane L3). The review: the natives make the JDK's checks in the
    // JDK's order (access, then the declaring class's initialization where
    // `MethodHandleAccessorFactory.newFieldAccessor` runs it, then the
    // accessor's conversion and receiver checks), raise HotSpot's throwables
    // with the JDK accessor's frames (`field_raise_chain`,
    // `field_class_init_chain`), and read the `override` flag the JDK's own
    // `setAccessible` writes; `--compatible` has run them against HotSpot on
    // the L3 field probes since wave 54.
    let field_door_kind = registry.field_door_kind(cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/reflect/Field",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        lang_class::native_field_get,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "set",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        lang_class::native_field_set,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getInt",
        "(Ljava/lang/Object;)I",
        lang_class::native_field_get_int,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getLong",
        "(Ljava/lang/Object;)J",
        lang_class::native_field_get_long,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getFloat",
        "(Ljava/lang/Object;)F",
        lang_class::native_field_get_float,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getDouble",
        "(Ljava/lang/Object;)D",
        lang_class::native_field_get_double,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getBoolean",
        "(Ljava/lang/Object;)Z",
        lang_class::native_field_get_boolean,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getByte",
        "(Ljava/lang/Object;)B",
        lang_class::native_field_get_byte,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getShort",
        "(Ljava/lang/Object;)S",
        lang_class::native_field_get_short,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getChar",
        "(Ljava/lang/Object;)C",
        lang_class::native_field_get_char,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setInt",
        "(Ljava/lang/Object;I)V",
        lang_class::native_field_set_int,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setLong",
        "(Ljava/lang/Object;J)V",
        lang_class::native_field_set_long,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setFloat",
        "(Ljava/lang/Object;F)V",
        lang_class::native_field_set_float,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setDouble",
        "(Ljava/lang/Object;D)V",
        lang_class::native_field_set_double,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setBoolean",
        "(Ljava/lang/Object;Z)V",
        lang_class::native_field_set_boolean,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setByte",
        "(Ljava/lang/Object;B)V",
        lang_class::native_field_set_byte,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setShort",
        "(Ljava/lang/Object;S)V",
        lang_class::native_field_set_short,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setChar",
        "(Ljava/lang/Object;C)V",
        lang_class::native_field_set_char,
        field_door_kind,
    );
    registry.register(
        "java/lang/reflect/AccessibleObject",
        "setAccessible",
        "(Z)V",
        lang_reflect::native_accessible_set_accessible, cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP on `AccessibleObject.getRoot()` only: that one is literally
    // `return null;` in the JDK — the base class has no `root` field.
    // The three subclass overrides return the receiver's `root` field, so W4
    // reads the field instead of hard-coding null (see
    // `native_reflect_get_root`). In practice the answer is unchanged — our
    // `copy()` natives below hand back the SAME object rather than minting a
    // linked copy, so `root` stays unset and `ReflectionFactory`'s "must be a
    // root" check still passes — but a reflective object that did acquire a
    // root now reports it instead of lying.
    // T15: Field.getRoot/getGenericSignature etc.
    registry.register(
        "java/lang/reflect/AccessibleObject",
        "getRoot",
        "()Ljava/lang/reflect/AccessibleObject;",
        |_ctx, _args| Ok(Some(Value::Object(None))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getRoot",
        "()Ljava/lang/reflect/Field;",
        native_reflect_get_root, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getRoot",
        "()Ljava/lang/reflect/Method;",
        native_reflect_get_root, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getRoot",
        "()Ljava/lang/reflect/Constructor;",
        native_reflect_get_root, cratonvm_native_api::NativeKind::Bridge,
    );
    // Our synthetic Field doesn't need a separate "copy" — reuse the same object.
    registry.register(
        "java/lang/reflect/Field",
        "copy",
        "()Ljava/lang/reflect/Field;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Method",
        "copy",
        "()Ljava/lang/reflect/Method;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "copy",
        "()Ljava/lang/reflect/Constructor;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))), cratonvm_native_api::NativeKind::Bridge,
    );
    // ReflectionFactory.copyField/copyMethod/copyConstructor: identity.
    registry.register(
        "jdk/internal/reflect/ReflectionFactory",
        "copyField",
        "(Ljava/lang/reflect/Field;)Ljava/lang/reflect/Field;",
        |_ctx, args| Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None)))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/reflect/ReflectionFactory",
        "copyMethod",
        "(Ljava/lang/reflect/Method;)Ljava/lang/reflect/Method;",
        |_ctx, args| Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None)))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/reflect/ReflectionFactory",
        "copyConstructor",
        "(Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;",
        |_ctx, args| Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None)))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/ProcessEnvironment",
        "environ",
        "()[[B",
        native_process_environment_environ,
        NativeKind::Bridge,
    );
    // ProcessImpl.init() is the JNI field-ID cache (the `initIDs` idiom under a
    // different name): it looks up the jfieldIDs the native process code later
    // pokes `handle`/`exitcode` through. Our ProcessBuilder/Process surface is
    // implemented in Rust against named fields, so there are no IDs to cache.
    registry.register(
        "java/lang/ProcessImpl",
        "init",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    // java/lang/ProcessEnvironment (Windows) — environmentBlock returns the
    // process's env vars as a null-separated string.  Build it from Rust.
    registry.register(
        "java/lang/ProcessEnvironment",
        "environmentBlock",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let mut block = String::new();
            // Interpreter round i5 wave 101, lane L1: `vars_os`, not `vars`,
            // which panics on the first variable that is not valid Unicode
            // (an unpaired surrogate in a Windows environment), so
            // `ProcessEnvironment.<clinit>` -- every `System.getenv` -- failed
            // where HotSpot's `GetEnvironmentStringsW` block reads it.
            for (k, v) in std::env::vars_os() {
                block.push_str(&k.to_string_lossy());
                block.push('=');
                block.push_str(&v.to_string_lossy());
                block.push('\0');
            }
            block.push('\0');
            Ok(Some(Value::Object(Some(ctx.create_string(&block)))))
        },
        NativeKind::Bridge,
    );
    // Executable/Method/Constructor accessors with the same synthetic layout.
    registry.register(
        "java/lang/reflect/Method",
        "getName",
        "()Ljava/lang/String;",
        lang_class::native_method_get_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getReturnType",
        "()Ljava/lang/Class;",
        lang_class::native_method_get_return_type, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getParameterTypes",
        "()[Ljava/lang/Class;",
        lang_class::native_method_get_parameter_types, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getModifiers",
        "()I",
        lang_class::native_method_get_modifiers, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        lang_class::native_method_get_declaring_class, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getParameterCount",
        "()I",
        lang_class::native_method_get_parameter_count, cratonvm_native_api::NativeKind::Bridge,
    );
    // WP2.2: Method.invoke is non-native in real JDK 25 (delegates to
    // MethodHandle-based DirectMethodHandleAccessor which performs primitive
    // box/unbox via .asType(genericMethodType(...))). Our MH .asType is a
    // no-op so primitive-arg methods receive Object references reinterpreted
    // as the primitive — yielding garbage values (cases 1-7, 23, 42-49 of
    // apps/method_invoke_probe). Registering the native here causes
    // try_stackless_invoke / vtable_fast / populate_invoke_cache to all
    // intercept the call BEFORE the JDK Java implementation runs, routing
    // through our native_method_invoke which uses coerce_arg_strict for
    // proper primitive-aware unboxing.
    registry.register(
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        lang_class::native_method_invoke, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getParameterTypes",
        "()[Ljava/lang/Class;",
        lang_class::native_constructor_get_parameter_types, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getModifiers",
        "()I",
        lang_class::native_constructor_get_modifiers, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        lang_class::native_constructor_get_declaring_class, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getParameterCount",
        "()I",
        lang_class::native_constructor_get_parameter_count, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "newInstance",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        lang_class::native_constructor_new_instance, cratonvm_native_api::NativeKind::Bridge,
    );
    // ScopedMemoryAccess.closeScope0(session, error) is the VM half of
    // `SharedSession.justClose()`, which has ALREADY set `state` to CLOSED
    // (-1, `STATE.setVolatile`) before it calls `closeScope`: HotSpot then
    // handshakes every thread so none is still inside a scoped access on
    // `session`. This VM's handshake is `panama::pe_close_handshake`: it
    // retires the JIT fast path's verdicts and waits for every access that
    // holds an `ffm_fast::AccessWindow` (round 12 wave 3, lane ffm). The
    // accesses that hold none (the bytecode non-scalar `*Internal` bodies) are
    // covered by the shared close's deferred free instead; see
    // `r20-shared-arena-close-does-not-wait-for-compiled-scoped-access-bodies`.
    //
    // nio20 wave 2, lane L1: NO state write here, as in HotSpot. This used to
    // read `args[0]` as the session and write its `state` -- but `closeScope0`
    // is an INSTANCE native (`native void closeScope0(..)`, javap of JDK 25),
    // so `args[0]` is the `ScopedMemoryAccess` singleton and `args[1]` the
    // session: the write was a no-op on a field-less receiver, and pointed at
    // the session it would have been wrong twice -- redundant for a real
    // `SharedSession` (`justClose` wrote it), and for this VM's modelled
    // session (every `Arena.ofShared()`, whose `close`/`justClose` are
    // force-dispatched to `foreign_ffm`, so this is never reached for one) a
    // `-1` reads as OPEN, the model's closed value being `0`.
    registry.register(
        "jdk/internal/misc/ScopedMemoryAccess",
        "closeScope0",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/misc/ScopedMemoryAccess$ScopedAccessError;)V",
        |ctx, _args| {
            // Round 12 wave 3 (lane ffm): HotSpot's handshake half. Retire every
            // FFM fast-path verdict and wait for element accesses already past
            // their check. Nothing is held across it (the wait is a blocking
            // region). See `panama::pe_close_handshake`.
            crate::panama::pe_close_handshake(ctx, true, &mut []);
            Ok(None)
        },
        NativeKind::Bridge,
    );
    let scoped_memory_access = "jdk/internal/misc/ScopedMemoryAccess";
    // The scalar Internal methods are reviewed intrinsics: their JDK bodies
    // check the session, perform the corresponding Unsafe scalar access, and
    // return. These callbacks keep that check and the shared-close window;
    // real sessions use their authoritative bytecode on a failing check.
    // The outer methods retain Bridge classification and their Java finally
    // blocks. The scope/thread/width oracle is vm/tests/scoped_byte_access.rs.
    for name in ["getByte", "getByteInternal"] {
        registry.register(
            scoped_memory_access,
            name,
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B",
            native_scoped_memory_get_byte,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    for name in ["putByte", "putByteInternal"] {
        registry.register(
            scoped_memory_access,
            name,
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V",
            native_scoped_memory_put_byte,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    for name in [
        "getShort",
        "getShortInternal",
        "getShortUnaligned",
        "getShortUnalignedInternal",
    ] {
        let descriptor = if name.contains("Unaligned") {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S"
        } else {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)S"
        };
        registry.register(
            scoped_memory_access,
            name,
            descriptor,
            native_scoped_memory_get_short,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    for name in [
        "putShort",
        "putShortInternal",
        "putShortUnaligned",
        "putShortUnalignedInternal",
    ] {
        let descriptor = if name.contains("Unaligned") {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V"
        } else {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)V"
        };
        registry.register(
            scoped_memory_access,
            name,
            descriptor,
            native_scoped_memory_put_short,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    for name in [
        "getInt",
        "getIntInternal",
        "getIntUnaligned",
        "getIntUnalignedInternal",
    ] {
        let descriptor = if name.contains("Unaligned") {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I"
        } else {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)I"
        };
        registry.register(
            scoped_memory_access,
            name,
            descriptor,
            native_scoped_memory_get_int,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    for name in [
        "putInt",
        "putIntInternal",
        "putIntUnaligned",
        "putIntUnalignedInternal",
    ] {
        let descriptor = if name.contains("Unaligned") {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V"
        } else {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)V"
        };
        registry.register(
            scoped_memory_access,
            name,
            descriptor,
            native_scoped_memory_put_int,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    for name in [
        "getLong",
        "getLongInternal",
        "getLongUnaligned",
        "getLongUnalignedInternal",
    ] {
        let descriptor = if name.contains("Unaligned") {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J"
        } else {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)J"
        };
        registry.register(
            scoped_memory_access,
            name,
            descriptor,
            native_scoped_memory_get_long,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    for name in [
        "putLong",
        "putLongInternal",
        "putLongUnaligned",
        "putLongUnalignedInternal",
    ] {
        let descriptor = if name.contains("Unaligned") {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V"
        } else {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)V"
        };
        registry.register(
            scoped_memory_access,
            name,
            descriptor,
            native_scoped_memory_put_long,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    // Like the other scalar Internal rows, char preserves its unsigned
    // result and the explicit unaligned byte order. Public wrappers remain Java.
    for name in [
        "getChar",
        "getCharInternal",
        "getCharUnaligned",
        "getCharUnalignedInternal",
    ] {
        let descriptor = if name.contains("Unaligned") {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C"
        } else {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)C"
        };
        registry.register(
            scoped_memory_access,
            name,
            descriptor,
            native_scoped_memory_get_char,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    for name in [
        "putChar",
        "putCharInternal",
        "putCharUnaligned",
        "putCharUnalignedInternal",
    ] {
        let descriptor = if name.contains("Unaligned") {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V"
        } else {
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)V"
        };
        registry.register(
            scoped_memory_access,
            name,
            descriptor,
            native_scoped_memory_put_char,
            if name.ends_with("Internal") {
                NativeKind::Intrinsic
            } else {
                NativeKind::Bridge
            },
        );
    }
    // Round nio20 wave 3, lane L5
    // (`r20-shared-close-java-cleanup-actions-race-unwindowed-scoped-bodies`):
    // the BULK Internal bodies are reviewed intrinsics too, so a shared close's
    // handshake waits for them before the close walk runs the session's Java
    // cleanup actions (a mapped file's unmap, a `reinterpret` cleanup's free).
    //
    // REVIEW (why each may shadow its real bytecode, AGENTS.md "only a
    // reviewed Intrinsic may win"). Every JDK 25 body is exactly
    // `try { if (s != null) s.checkValidStateRaw(); [if (t != null)
    // t.checkValidStateRaw();] <one call>; } finally { reachabilityFence(s..); }`
    // (javap -p -c). The callbacks keep, in that order and inside ONE
    // `ffm_fast::AccessWindow`: both null-guarded checks
    // (`p67_check_scoped_session_arg`: a real session runs its own
    // `checkValidStateRaw`, so a closed or wrong-thread real session throws
    // the JDK's `ScopedAccessError` for the public wrapper to translate; a
    // modelled one throws the translated `IllegalStateException` /
    // `WrongThreadException` directly, as the scalar intrinsics do), then the
    // one call, reached at the native the bytecode itself reaches
    // (`copyMemory0` / `copySwapMemory0` / `setMemory0` bodies, and the
    // `ArraysSupport.vectorizedMismatch` intrinsic, so a mismatch answers the
    // same value on both routes). The sessions stay native arguments for the
    // whole call, which is the `reachabilityFence`. Not kept: the `Unsafe`
    // Java-level `checkSize` / `checkPrimitivePointer` /
    // `copySwapMemoryChecks` argument checks, which no `ScopedMemoryAccess`
    // caller can fail (every caller is a JDK segment or buffer that bounds-
    // and alignment-checked its operands first; the class is not exported).
    // The public wrappers stay `Bridge` (retired: their Java `catch
    // (ScopedAccessError)` is authoritative). With the IR inline resolver the
    // registration also refuses the wave-1 fence-finally splice of these four
    // bodies (`native-shadow`), so no compiled caller reaches an unwindowed
    // copy of them. Kill switch `CRATONVM_FFM_WINDOWED_SCOPED_BULK=0`.
    let windowed_bulk = crate::ffm_fast::windowed_scoped_bulk_enabled();
    let bulk_kind = if windowed_bulk {
        NativeKind::Intrinsic
    } else {
        NativeKind::Bridge
    };
    let copy_descriptor = "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V";
    registry.register(
        scoped_memory_access,
        "copyMemory",
        copy_descriptor,
        native_scoped_memory_copy_memory,
        NativeKind::Bridge,
    );
    registry.register(
        scoped_memory_access,
        "copyMemoryInternal",
        copy_descriptor,
        native_scoped_memory_copy_memory,
        bulk_kind,
    );
    if windowed_bulk {
        registry.register(
            scoped_memory_access,
            "copySwapMemoryInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJJ)V",
            native_scoped_memory_copy_swap_memory,
            NativeKind::Intrinsic,
        );
        registry.register(
            scoped_memory_access,
            "setMemoryInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJB)V",
            native_scoped_memory_set_memory,
            NativeKind::Intrinsic,
        );
        registry.register(
            scoped_memory_access,
            "vectorizedMismatchInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JII)I",
            native_scoped_memory_vectorized_mismatch,
            NativeKind::Intrinsic,
        );
    }
    // Default ON since nio20 wave 5 (`CRATONVM_FFM_WINDOWED_SCOPED_TWINS=0` off): every other
    // session-checked Internal body runs its own bytecode inside a window.
    // See `scoped_twins`.
    if crate::ffm_fast::windowed_scoped_twins_enabled() {
        crate::scoped_twins::register_windowed_scoped_twins(registry);
    }

    // T15: ReflectionFactory.getExecutableSharedParameterTypes — the real
    // JDK bytecode dereferences the `langReflectAccess` field which is set
    // by `java.lang.reflect.AccessibleObject.<clinit>`.  When user code
    // (e.g. JBoss Modules) loads ReflectionFactory before AccessibleObject,
    // the field is null and the bytecode NPEs.  Bypass the whole chain by
    // dispatching directly to `Executable.getParameterTypes()` via
    // invoke_virtual, which works regardless of SharedSecrets state.
    registry.register(
        "jdk/internal/reflect/ReflectionFactory",
        "getExecutableSharedParameterTypes",
        "(Ljava/lang/reflect/Executable;)[Ljava/lang/Class;",
        |ctx, args| {
            let exec = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some(
                            "ReflectionFactory.getExecutableSharedParameterTypes: executable is null"
                                .to_string(),
                        ),
                    }
                    .into());
                }
            };
            // Dispatch virtually to the Executable's concrete getParameterTypes.
            ctx.invoke_virtual(exec, "getParameterTypes", "()[Ljava/lang/Class;", &[])
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP: correct constant (the null-vs-empty-array distinction below is
    // load-bearing, not incidental).
    // Same for the newer getExecutableTypeAnnotationBytes — most bootstrap
    // code that touches annotations on an Executable ends up here. Return
    // NULL (not an empty byte[]) when no RuntimeVisibleTypeAnnotations are
    // present: `sun.reflect.annotation.TypeAnnotationParser.parseTypeAnnotations`
    // treats `null` as "no annotations" (returns the empty result) but does
    // `ByteBuffer.wrap(bytes).getShort()` on a non-null buffer — so a
    // zero-length array throws `BufferUnderflowException`. ByteBuddy's mock
    // creation reads `TypeVariable.getDeclaredAnnotations()` on the mocked
    // type's generic methods, which routes here; the empty-array form broke
    // Mockito's inline mock maker ("could not instrument all classes within
    // the mock's type hierarchy").
    registry.register(
        "jdk/internal/reflect/ReflectionFactory",
        "getExecutableTypeAnnotationBytes",
        "(Ljava/lang/reflect/Executable;)[B",
        |_ctx, _args| Ok(Some(Value::Object(None))), cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP: `null` is the JDK's own "no constant pool available" answer and
    // every caller null-checks it. CratonVM answers reflective annotation and
    // signature queries from its own class-file store (`class_annotations` /
    // `method_annotations` / `class_signature` on `NativeContext`), so the
    // `ConstantPool`-walking parser path is never the one that runs;
    // materialising a real `jdk.internal.reflect.ConstantPool` would buy
    // nothing that surface does not already answer.
    registry.register(
        "jdk/internal/reflect/ReflectionFactory",
        "getConstantPool",
        "(Ljava/lang/Class;)Ljdk/internal/reflect/ConstantPool;",
        |_ctx, _args| Ok(Some(Value::Object(None))), cratonvm_native_api::NativeKind::Bridge,
    );

    // --- jdk/internal/misc/Unsafe additional methods ---
    // getNextThreadIdOffset calls Unsafe.objectFieldOffset internally.
    // Ensure we have it covered (register_unsafe_natives handles most).

    // --- Throwable additional signatures ---
    // JDK 9+: Throwable.getStackTraceDepth / getStackTraceElement are used
    // by Throwable.getOurStackTrace() on older JDKs when a backtrace object
    // exists. Use the real implementations (which read the identity-hash-keyed
    // trace store) rather than null-stubs — the stubs forced depth 0 / null
    // frames in real-JDK mode, where `register_synthetic_overrides` (the only
    // other place these were wired correctly) is never called.
}
