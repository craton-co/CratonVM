#[allow(unused_imports)]
use super::*;

/// `jdk.internal.loader.BuiltinClassLoader.findResourceAsStream(String,String)`.
/// Reviewed intrinsic: the JDK body first, then an exact system-module lookup
/// when Compatible's module graph has no Java nameToModule entry. The Class
/// caller has already applied module encapsulation. Never search another
/// module or an application class path, and retain the JDK body in JdkOnly.
pub fn builtin_classloader_find_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = match (args.first(), args.get(1), args.get(2)) {
        (
            Some(Value::Object(Some(loader))),
            Some(Value::Object(Some(module))),
            Some(Value::Object(Some(name))),
        ) => {
            let kind = builtin_loader_kind(
                ctx.class_name_of_id(ctx.class_id_of_object(*loader))
                    .as_deref(),
            );
            ctx.read_string(*module)
                .zip(ctx.read_string(*name))
                .map(|(module, name)| (kind, module, name))        }
        _ => None,
    };
    // `mn == null` (the unnamed module's `Module.getResourceAsStream`): the
    // application loader's class path only (`findResourceOnClassPath`).
    let unnamed_app_name = match (args.first(), args.get(1), args.get(2)) {
        (
            Some(Value::Object(Some(loader))),
            Some(Value::Object(None)),
            Some(Value::Object(Some(n))),
        ) => {
            let kind = builtin_loader_kind(
                ctx.class_name_of_id(ctx.class_id_of_object(*loader))
                    .as_deref(),
            );
            if kind == Some(BuiltinLoaderKind::Application) {
                ctx.read_string(*n)            } else {
                None            }        }
        _ => None,
    };
    let loader = obj_arg(args, 0)?;
    let original = ctx.invoke_virtual_bytecode_only(
        loader,
        "findResourceAsStream",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/InputStream;",
        &args[1..],
    )?;
    if ctx.is_jdk_only() || matches!(original, Some(Value::Object(Some(_)))) {
        return Ok(original);
    }
    // Interpreter round i4 wave 98, lane L3 (`L3W98ResourcesByLoader` row
    // `unnamed module stream L3W98ResourcesByLoader.class`: HotSpot the
    // bytes, `--compatible` null): the JDK body reads the loader's
    // `URLClassPath`, which `--compatible` leaves empty. Answer the VM's
    // application class path, only when no boot or extension entry shadows
    // the name in the VM's flat lookup (then `find_resource` is the
    // application copy); a shadowed name keeps the body's null.
    if let Some(name) = unnamed_app_name {
        if !name.is_empty()
            && !name.ends_with('/')
            && crate::lang_class::t19_h10_validate_resource_name_pub(&name).is_some()
            && !ctx.find_resource_urls_in_segment(&name, 2).is_empty()
            && ctx.find_resource_urls_in_segment(&name, 0).is_empty()
            && ctx.find_resource_urls_in_segment(&name, 1).is_empty()
        {
            if let Some(bytes) = ctx.find_resource(&name) {
                return Ok(Some(Value::Object(Some(
                    crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes)?,
                ))));
            }
        }
        return Ok(original);
    }
    let Some((Some(kind), module, name)) = input else {
        return Ok(original);
    };
    let Some(sets) = jdk_builtin_module_sets(ctx) else {
        return Ok(original);
    };
    let owns = match kind {
        BuiltinLoaderKind::Boot => sets.boot.contains(&module),
        BuiltinLoaderKind::Platform => sets.platform.contains(&module),
        BuiltinLoaderKind::Application => {
            !sets.boot.contains(&module) && !sets.platform.contains(&module)
        }
    };
    if !owns {
        return Ok(original);
    }
    match system_module_resource_bytes(ctx, &module, &name) {
        Some(bytes) => Ok(Some(Value::Object(Some(
            crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes)?,
        )))),
        // Interpreter round i4 wave 98, lane L3: a named `--module-path`
        // module is the application loader's; its own `ModuleReader`.
        None if kind == BuiltinLoaderKind::Application => {
            module_path_module_resource_stream(ctx, &module, &name).unwrap_or(Ok(original))
        }
        None => Ok(original),
    }
}

/// Synthetic-JDK `ClassLoader.getDefinedPackage(String)`.
///
/// Delegates to the real-JDK implementation rather than answering an
/// unconditional `null`: that stub is what made every package-name
/// `SpringApplication` source fail as `IllegalArgumentException: Invalid
/// source '<pkg>'` (`BeanDefinitionLoader.findPackage` returns exactly this
/// probe's result), and there is no reason for the two boot modes to disagree.
/// `getDefinedPackages`/`getPackages` below stay empty — see the note on
/// `lang_class::i2_classloader_get_defined_packages`.
pub fn cl_get_defined_package(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_class::i2_classloader_get_defined_package(ctx, args)
}

pub fn cl_get_defined_packages(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // `Package[0]`, the declared type (i8-L2), not `Object[0]`.
    let component = crate::lang_class::loading_component_id(ctx, "java/lang/Package");
    let empty = ctx.new_ref_array(component, 0);
    Ok(Some(Value::Object(Some(empty))))
}

pub fn cl_set_default_assertion_status(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

pub fn cl_register_as_parallel_capable(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Static method (invokestatic, descriptor ()Z).  The real JDK uses
    // getCallerClass() to find which ClassLoader subclass is being registered.
    // We don't enforce parallel-capability checks, so just return true.
    Ok(Some(Value::Int(1)))
}

/// `ClassLoader.isRegisteredAsParallelCapable()Z`.
///
/// WAVE-3 FIX (found by cross-package audit): this registration WINS over the
/// `phases_late::register_p61_classloader` copy (both are reachable only from
/// `register_synthetic_overrides`; lib.rs calls the classloader one LAST), and
/// it used to read a field that was written `0` at every initialiser and never
/// written `1` anywhere. Meanwhile `registerAsParallelCapable()` (above, and
/// `deprecated_internal.rs`, and `classloader_real.rs`) answers an
/// unconditional `true`. A loader that had just successfully registered was
/// then told it had not — the register/query pair contradicted itself.
///
/// The four `ClassLoader` initialisers now seed `parallel_capable = true`,
/// which is the truthful answer for this VM: every synthetic loader IS
/// parallel-capable, because class definition serialises on the VM's own
/// global class-registry lock rather than on the loader object, so no loader
/// can deadlock another by loading concurrently. The state stays per-loader
/// (not a constant) so a future per-loader model can flip it back to 0.
///
/// L1 moved that state out of slot 4 and into [`LoaderMeta`]: on a real JDK
/// image slot 4 is `java.lang.ClassLoader.parallelLockMap`, so the seed write
/// was destroying a JDK field and the read was answering its fallback by
/// accident. `phases_late`'s shadowed twin reads through the same accessor.
pub fn cl_is_registered_as_parallel_capable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Loader this VM never recorded, or one on the real JDK layout (where
    // slot 4 is `parallelLockMap`, not our flag): fall back to the same `true`
    // the registration call reports, rather than contradicting it.
    let val = loader_parallel_capable_of(ctx, this).unwrap_or(1);
    Ok(Some(Value::Int(val)))
}

// ---------------------------------------------------------------------------
// java.net.URLClassLoader natives
// ---------------------------------------------------------------------------

/// Extract filesystem path from a URL object (tries field 3 = path, field 5 = full string).
///
/// Returns one of two shapes depending on the URL form:
///   * `file:/X/foo.jar`            → `X/foo.jar`            (plain JAR / directory)
///   * `jar:file:/X/foo.jar!/sub/`  → `X/foo.jar!/sub/`      (JAR with internal prefix)
///
/// The `!/<prefix>/` form is preserved so `ClassPath::add_path` can build a
/// `NestedDirectory` entry pointing at the right place inside the outer JAR.
/// Without this, DaCapo's `Harness` (which builds a URLClassLoader rooted at
/// `harness/` inside the launcher JAR) silently dropped the entry from the
/// dynamic classpath and every subsequent `loadClass` returned CNFE.
pub fn extract_url_path(ctx: &dyn NativeContext, url_obj: ObjectRef) -> Option<String> {
    // For `jar:` URLs, the synthetic-URL builder puts the full
    // `file:/.../foo.jar!/sub/` (without the `jar:` prefix) into both
    // the `file` and `path` named fields. For plain `file:` URLs, slot 3
    // (path) is just `/X/foo.jar` (or on Windows `/C:/X/foo.jar`). To
    // distinguish the two cases we consult the FULL spec (slot 5) first
    // when present, so we know whether to keep the JAR-internal suffix.
    // Real-JDK URL objects expose these as named instance fields.  Reading
    // only the synthetic numeric slots loses constructor URLs for ordinary
    // URLClassLoader instances and makes their local class path appear empty.
    let read_field = |name: &str, slot: usize| match ctx.get_field_by_name(url_obj, name) {
        Value::Object(Some(value)) => ctx.read_string(value),
        _ => match ctx.get_field(url_obj, slot) {
            Value::Object(Some(value)) => ctx.read_string(value),
            _ => None,
        },
    };
    let full_spec = read_field("file", 5);
    let path_field = read_field("path", 3);

    // Pick the most descriptive string: if the path field encodes the
    // `!/<prefix>/` shape (which `build_synthetic_url` does — see
    // jboss_module_loader::build_synthetic_url where field "path" is
    // set to the post-`jar:` remainder), prefer it; otherwise fall back
    // to the full spec; otherwise the raw string read off the object.
    let raw = path_field
        .or(full_spec)
        .or_else(|| ctx.read_string(url_obj))?;

    // Normalise: strip a leading `jar:` (so `jar:file:/X!/sub/` collapses
    // to `file:/X!/sub/`), then strip the `file:` scheme. We keep the
    // `!/<prefix>/` suffix intact for `ClassPath::add_path` to interpret.
    //
    // Only the FIRST `/!` is the genuine outer-jar/nested-entry boundary
    // marker (from `getJarReference`'s `"nested:" + jarFilePath + "/!" +
    // nestedEntryName`). A `.replace` of every occurrence also mangles a
    // directory-shaped nested entry name (e.g. Spring Boot's
    // `JarUrl.create(file, "BOOT-INF/classes/")`, whose spec is
    // `nested:<jar>/!BOOT-INF/classes/!/`): the entry name's own trailing
    // `/` immediately followed by the URL's separate trailing `!/` root
    // marker forms a SECOND, spurious `/!` match, which swaps into the
    // entry name and eats its trailing slash (`BOOT-INF/classes/!/` ->
    // `BOOT-INF/classes!//`), silently emptying `ClassPath`'s nested-prefix
    // scan (`parse_jar_subdir_spec` never matches any real zip entry).
    let p = raw
        .strip_prefix("jar:")
        .or_else(|| raw.strip_prefix("nested:"))
        .unwrap_or(&raw)
        .replacen("/!", "!/", 1);
    let p = p.strip_prefix("file:").unwrap_or(&p).to_string();
    let p = p.strip_prefix("//").unwrap_or(&p).to_string();
    // `File.toURI().toURL()` percent-encodes reserved/space characters in the
    // path (e.g. a directory named `app location` becomes `app%20location`).
    // Real `URLClassPath` decodes this back (`new File(url.toURI())`) before
    // touching the filesystem; do the same here, mirroring the sibling
    // `jar:file:` decode in `net_phase_e::uri_percent_decode`. Decoding after
    // the `/!`-marker normalisation above keeps the jar-boundary detection on
    // the original encoded text.
    let p = crate::net_phase_e::uri_percent_decode(&p);
    // A root archive URL ends in `!/` (for example
    // `jar:file:src/test/resources/jars/app.jar!/`).  There is no virtual
    // subdirectory to retain in that form: it denotes the archive's root.
    // Leaving the marker in the filesystem token makes `ClassPath::new`
    // probe the non-existent path `app.jar!/`, so a receiver-local
    // URLClassLoader sees neither resources nor classes from its own JAR.
    // Keep non-root `!/prefix/` specifications intact; those become a
    // `NestedDirectory` classpath entry below.
    let p = if p.ends_with("!/") && p.find("!/") == Some(p.len() - 2) {
        p[..p.len() - 2].to_string()
    } else {
        p
    };
    // Windows: `File.toURI().toURL()` yields `file:/C:/dir/...`, so the
    // extracted path is `/C:/dir/...` — a leading slash *before* the
    // drive letter. `PathBuf::from("/C:/...")` does not resolve on
    // Windows (`is_dir()` / `exists()` both fail), which made every
    // directory/jar URL silently skipped by `ClassPath::add_path`.
    // Strip the spurious leading slash when followed by a drive letter.
    let bytes = p.as_bytes();
    let p = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        p[1..].to_string()
    } else {
        p
    };
    Some(p)
}

// ---------------------------------------------------------------------------
// The `URLClassPath$Loader` walk index (interpreter round i2 wave 54, lane L5;
// `docs/feature-designs/class-resolution-and-caching.md`).
// ---------------------------------------------------------------------------

/// The generic loader of the JDK's `URLClassPath`, the one a URL whose
/// `jar:` handler is not the JDK's own gets (`URLClassPath.getLoader(URL)`).
pub const URLCLASSPATH_LOADER: &str = "jdk/internal/loader/URLClassPath$Loader";

/// Spring Boot's `jar:` handler (3.2 and later), whose `JarUrlConnection.open`
/// answers a `jar:nested:<outer>/!<inner>!/<name>` URL from the nested jar's
/// central directory (`hasEntry`: `name` or `name + "/"`).
pub const SPRING_NESTED_JAR_HANDLER: &str = "org/springframework/boot/loader/net/protocol/jar/Handler";

/// Reviewed `NativeKind::Intrinsic` on `URLClassPath$Loader.getResource(String)`
/// (`CRATONVM_NESTED_JAR_WALK_INDEX`, registered where the `URLClassLoader`
/// family runs the JDK's bodies). It changes which loaders the JDK's walk
/// pays for, never what one answers: `null` without running the body when
/// [`nested_jar_walk_index_says_absent`] holds, the JDK body otherwise.
///
/// Why `null` is the body's own answer there: the body builds
/// `new URL(base, ParseUtil.encodePath(name, false))` and opens it; Spring's
/// `Handler` resolves the relative name to `<base-file><name>` (no `.` /
/// `..` segment, no `#`, `?`, `:`, `%` or `!` -- [`nested_jar_walk_name_is_plain`]),
/// `JarUrlConnection.open` finds no entry `name` / `name + "/"` in the nested
/// jar and returns a not-found connection, whose `getJarFile()` /
/// `getInputStream()` throw `FileNotFoundException`, which the body catches
/// into `null` (`getResource`) -- before `jarfile` is written. The only state
/// the skipped call would have left is Spring's own cache entry for the nested
/// jar (`UrlJarFiles`), which a later present name creates the same way.
/// JarLoader and FileLoader override both methods with their own bytecode, so
/// they never reach this.
pub fn nested_jar_walk_loader_get_resource(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    nested_jar_walk_loader_lookup(
        ctx,
        args,
        "getResource",
        "(Ljava/lang/String;)Ljdk/internal/loader/Resource;",
        false,
    )
}

/// [`nested_jar_walk_loader_get_resource`] with the stamped walk index
/// (`CRATONVM_NESTED_JAR_WALK_INDEX_STAMP`): a row whose fat jar was rewritten
/// on disk since it was built is rebuilt before it answers
/// ([`nested_jar_entry_index`]). Interpreter round i2 wave 65, lane L5.
pub fn nested_jar_walk_loader_get_resource_stamped(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    nested_jar_walk_loader_lookup(
        ctx,
        args,
        "getResource",
        "(Ljava/lang/String;)Ljdk/internal/loader/Resource;",
        true,
    )
}

/// [`nested_jar_walk_loader_get_resource`]'s twin on
/// `URLClassPath$Loader.findResource(String)` (`URL`): the body's miss path is
/// the same connection's `getInputStream()` throwing, caught into `null`.
pub fn nested_jar_walk_loader_find_resource(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    nested_jar_walk_loader_lookup(
        ctx,
        args,
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        false,
    )
}

/// [`nested_jar_walk_loader_find_resource`] with the stamped walk index
/// (see [`nested_jar_walk_loader_get_resource_stamped`]).
pub fn nested_jar_walk_loader_find_resource_stamped(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    nested_jar_walk_loader_lookup(
        ctx,
        args,
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        true,
    )
}

pub fn nested_jar_walk_loader_lookup(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
    stamped: bool,
) -> MethodCallResult {
    if nested_jar_walk_index_says_absent(ctx, args, stamped) {
        ctx.note_nested_jar_walk(true);
        return Ok(Some(Value::Object(None)));
    }
    ctx.note_nested_jar_walk(false);
    // The JDK's own body, exactly as the call would have run it: the receiver
    // IS a generic `Loader` (the subclasses override), so `invokespecial`
    // binding on `Loader` is the method virtual dispatch selected.
    //
    // gci i18/vp: a tail call (`continuation_pin::tail_call`); the four
    // registered callbacks above pass their own name and descriptor and
    // return this answer. The body opens a URL connection through the URL's
    // handler (application code, a socket for a remote class path), which
    // may park.
    cratonvm_native_api::continuation_pin::tail_call(|| {
        ctx.invoke_special_bytecode_only(URLCLASSPATH_LOADER, method, descriptor, args)
    })
}

/// Does the VM's index of the receiver loader's nested jar prove the JDK body
/// answers `null` for `args[1]`? Every condition must hold; any doubt answers
/// `false` (the body runs):
///
/// * the receiver's `base` URL carries Spring Boot's `jar:` handler and no
///   `ref`, and its `file` is `nested:<outer>/!<inner>!/` with
///   an `<inner>` that names a jar, not a directory ([`nested_jar_spec_parts`]);
/// * the name is plain ([`nested_jar_walk_name_is_plain`]), so the URL the
///   body builds names exactly `<inner>!/<name>`;
/// * the index of `<inner>` inside `<outer>` was built
///   ([`nested_jar_entry_index`]; with `stamped`, rebuilt first if the
///   outer file changed on disk) and holds neither `name` nor `name + "/"`.
///   The index lists every `META-INF/versions/<N>/<name>` entry under
///   `<name>` too: a Multi-Release nested jar is served through its versioned
///   view (wave-54 follow-up: spring-core 7 keeps
///   `ClassFileMetadataReaderFactory` only there, and the skip broke the
///   `--real-jdk` fat jar).
pub fn nested_jar_walk_index_says_absent(
    ctx: &dyn NativeContext,
    args: &[Value],
    stamped: bool,
) -> bool {
    let (Some(Value::Object(Some(loader))), Some(Value::Object(Some(name_obj)))) =
        (args.first(), args.get(1))
    else {
        return false;
    };
    let Value::Object(Some(base)) = ctx.get_field_by_name(*loader, "base") else {
        return false;
    };
    let Some(file) = spring_nested_url_file(ctx, base) else {
        return false;
    };
    let Some(name) = ctx.read_string(*name_obj) else {
        return false;
    };
    if !nested_jar_walk_name_is_plain(&name) {
        return false;
    }
    let Some(index) = nested_jar_entry_index(ctx, &file, stamped) else {
        return false;
    };
    !index.contains(name.as_str()) && !index.contains(&format!("{name}/"))}

/// A resource name whose URL the JDK body and Spring's handler resolve to
/// exactly `<base>!/<name>`: printable ASCII, no empty / `.` / `..` segment,
/// no leading or trailing `/`, and none of `# ? : % ! \` (a ref, a query, a
/// scheme, an escape, a jar separator, a Windows separator).
pub fn nested_jar_walk_name_is_plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 4096
        && name
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'#' | b'?' | b':' | b'%' | b'!' | b'\\'))
        && name
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// `nested:<outer>/!<inner>!/` -> the outer archive's file-system path
/// (percent-decoded as UTF-8, a Windows drive's leading `/` dropped) and the
/// inner jar's entry name. `None` for any other shape, a directory entry
/// (`BOOT-INF/classes/`), or an ambiguous one (a `!` or `%` in the entry name,
/// a `!` in the outer path: Spring writes the outer's `!` as `%21`).
pub fn nested_jar_spec_parts(file: &str) -> Option<(String, String)> {
    let rest = file.strip_prefix("nested:")?.strip_suffix("!/")?;
    let separator = rest.find("/!")?;
    let (outer_raw, inner) = (&rest[..separator], &rest[separator + 2..]);
    if outer_raw.is_empty()
        || outer_raw.contains('!')
        || inner.is_empty()
        || inner.ends_with('/')
        || inner.contains(['!', '%', '#', '?', '\\'])
    {
        return None;
    }
    let outer = percent_decode_utf8(outer_raw)?;
    let bytes = outer.as_bytes();
    let outer = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        outer[1..].to_string()
    } else {
        outer
    };
    Some((outer, inner.to_string()))
}

/// `%XX` escapes decoded as UTF-8 bytes (`URI.getRawPath`'s encoding); `None`
/// for a malformed escape or a non-UTF-8 result.
pub fn percent_decode_utf8(input: &str) -> Option<String> {
    if !input.contains('%') {
        return Some(input.to_string());
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = input.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The entry names of the nested jar a `nested:<outer>/!<inner>!/` spec names
/// (`ClassPath::nested_archive_entry_index`), memoised by the spec in THIS
/// VM's [`ArchiveBytesCaches`] (`walk`). `None` (also memoised) when the spec
/// is not that shape or the jar cannot be read: the walk then runs the JDK
/// body for every name of that loader.
///
/// Wave 54 kept this in a process-wide cache; interpreter round i2 wave 65,
/// lane L5 moved it per VM (AGENTS.md: no process globals), so one VM's row
/// is never another VM's answer. With `stamped`
/// (`CRATONVM_NESTED_JAR_WALK_INDEX_STAMP`, default on, `=0` off) every hit re-stats
/// the outer archive and rebuilds a row whose fat jar was rewritten since
/// (`i63-L5-the-nested-jar-walk-index-never-notices-a-rewritten-fat-jar`);
/// without it a row is kept until the map is cleared, as before. A context
/// with no per-VM slot (mock contexts) builds the index for this ask only.
pub fn nested_jar_entry_index(
    ctx: &dyn NativeContext,
    spec: &str,
    stamped: bool,
) -> Option<Arc<cratonvm_classloading::NestedEntryNameIndex>> {
    let caches = archive_bytes_caches(ctx);
    // `CRATONVM_NESTED_JAR_SHARED_OUTER` (wave 66): the outer archive comes
    // from the VM's one open of that fat jar ([`shared_outer_archive`]).
    let shared = caches.filter(|_| ctx.nested_jar_shared_outer());
    // `CRATONVM_NESTED_JAR_OUTER_INDEX` (wave 67): the outer central
    // directory comes from the VM's one parse of that fat jar, held without
    // its bytes ([`shared_outer_index`]); preferred over `shared`.
    let indexed = caches.filter(|_| ctx.nested_jar_outer_index());
    // Built outside any lock: it maps the outer archive and reads two
    // central directories (one, with a shared outer; with an outer index it
    // reads the one nested jar's bytes and its central directory).
    let build = || -> Option<Arc<cratonvm_classloading::NestedEntryNameIndex>> {
        let (outer, inner) = nested_jar_spec_parts(spec)?;
        // `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` (wave 69): a fat jar on the
        // VM's application class path lends its own parse ([`class_path_outer`]).
        if let Some(outer) = class_path_outer(ctx, &outer) {
            return cratonvm_classloading::ClassPath::nested_archive_entry_index_in(&outer, &inner)
                .map(Arc::new);
        }
        match (indexed, shared) {
            (Some(caches), _) => {
                let outer = shared_outer_index(caches, &outer)?;
                cratonvm_classloading::ClassPath::nested_archive_entry_index_in_index(
                    &outer, &inner,
                )
                .map(Arc::new)
            }
            (None, Some(caches)) => {
                let outer = shared_outer_archive(caches, &outer)?;
                cratonvm_classloading::ClassPath::nested_archive_entry_index_in(&outer, &inner)
                    .map(Arc::new)
            }
            (None, None) => cratonvm_classloading::ClassPath::nested_archive_entry_index(
                std::path::Path::new(&outer),
                &inner,
            )
            .map(Arc::new),
        }
    };
    // `CRATONVM_NESTED_JAR_REFUSAL_STAMP` (wave 68): a refusal remembers the
    // outer file's stamp and is retried once it changed ([`memoised_row`]).
    let outer_path =
        || nested_jar_spec_parts(spec).map(|(outer, _)| std::path::PathBuf::from(outer));
    let refusal_path = ctx
        .nested_jar_refusal_stamp()
        .then_some(&outer_path as &dyn Fn() -> Option<std::path::PathBuf>);
    let note = |event| ctx.note_nested_jar_row(event);
    match caches {
        Some(caches) => memoised_walk_index_noted(
            &caches.walk,
            spec,
            NESTED_JAR_INDEX_CACHE_LIMIT,
            stamped,
            refusal_path,
            &note,
            build,
        ),
        None => build(),
    }
}

/// THIS VM's one open of the fat jar at `outer`
/// (`CRATONVM_NESTED_JAR_SHARED_OUTER`; interpreter round i2 wave 66, lane
/// L5): its mapping (read into memory on Windows) and parsed central
/// directory, kept in [`ArchiveBytesCaches`] (`outer`) and shared by every
/// nested jar the walk index or the bytes index builds a row for, so N nested
/// specs cost one outer parse instead of N (2N with both indexes).
///
/// A held open is reused only after a `stat` finds the file unchanged (same
/// length and modification time); a changed file is reopened and replaces
/// the row, so a row built from it carries the stamp of the file as it now
/// is -- the wave-65 `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP` rule then
/// applies to that row unchanged. The stat is paid on a row BUILD only
/// (a walk-index miss), never on a walk-index hit. A refused open is not
/// remembered (the per-spec rows remember their own refusals). The map is
/// cleared when it reaches [`OUTER_ARCHIVE_CACHE_LIMIT`] fat jars.
pub fn shared_outer_archive(
    caches: &ArchiveBytesCaches,
    outer: &str,
) -> Option<Arc<cratonvm_classloading::OuterArchive>> {
    let hit = caches.outer.lock().get(outer).cloned();
    // A stat, outside the lock.
    if let Some(archive) = hit.filter(|archive| archive.unchanged()) {
        return Some(archive);
    }
    // Opened outside the lock: it maps (or reads) the file and parses its
    // central directory. Two threads racing here each open it once; the
    // last insert wins and both answers are the same file's.
    let opened =
        cratonvm_classloading::OuterArchive::open(std::path::Path::new(outer)).map(Arc::new);
    let mut guard = caches.outer.lock();
    match &opened {
        Some(archive) => {
            if guard.len() >= OUTER_ARCHIVE_CACHE_LIMIT && !guard.contains_key(outer) {
                guard.clear();
            }
            guard.insert(outer.to_string(), Arc::clone(archive));
        }
        None => {
            guard.remove(outer);
        }
    }
    opened
}

/// The bound of [`shared_outer_archive`]'s cache: each row holds one fat
/// jar's mapping (its bytes on Windows) and parsed central directory.
pub const OUTER_ARCHIVE_CACHE_LIMIT: usize = 64;

/// One open outer fat jar per file-system path
/// ([`shared_outer_archive`]).
pub type OuterArchiveCache =
    std::collections::HashMap<String, Arc<cratonvm_classloading::OuterArchive>>;

/// THIS VM's one parse of the fat jar at `outer`'s central directory, held
/// WITHOUT the outer bytes (`CRATONVM_NESTED_JAR_OUTER_INDEX`; interpreter
/// round i2 wave 67, lane L5; the successor of [`shared_outer_archive`]):
/// kept in [`ArchiveBytesCaches`] (`outer_index`) and shared by every nested
/// jar the walk index or the bytes index builds a row for. Each row build
/// reads its ONE nested jar with a bounded positioned read
/// (`OuterArchiveIndex`), so the VM holds no mapping, no open handle and no
/// resident copy of the fat jar between asks: a fat jar truncated, rewritten
/// or deleted under a running VM is a refused read (the body runs), never a
/// `SIGBUS`, and Windows can delete or replace it.
///
/// Reuse and replacement follow [`shared_outer_archive`]: a held parse is
/// reused only after a `stat` finds the file unchanged (the read re-checks
/// the stamp on the open handle, before and after), a changed file is
/// reparsed and replaces the row, a refused parse is not remembered, and the
/// map is cleared at [`OUTER_ARCHIVE_CACHE_LIMIT`] fat jars.
pub fn shared_outer_index(
    caches: &ArchiveBytesCaches,
    outer: &str,
) -> Option<Arc<cratonvm_classloading::OuterArchiveIndex>> {
    let hit = caches.outer_index.lock().get(outer).cloned();
    // A stat, outside the lock.
    if let Some(index) = hit.filter(|index| index.unchanged()) {
        return Some(index);
    }
    // Parsed outside the lock (the file is open only while it is parsed).
    // Two threads racing here each parse it once; the last insert wins.
    let opened =
        cratonvm_classloading::OuterArchiveIndex::open(std::path::Path::new(outer)).map(Arc::new);
    let mut guard = caches.outer_index.lock();
    match &opened {
        Some(index) => {
            if guard.len() >= OUTER_ARCHIVE_CACHE_LIMIT && !guard.contains_key(outer) {
                guard.clear();
            }
            guard.insert(outer.to_string(), Arc::clone(index));
        }
        None => {
            guard.remove(outer);
        }
    }
    opened
}

/// One outer central directory (no bytes) per file-system path
/// ([`shared_outer_index`]).
pub type OuterIndexCache =
    std::collections::HashMap<String, Arc<cratonvm_classloading::OuterArchiveIndex>>;

/// The VM's application class path's own parse of the fat jar at `outer`
/// (`CRATONVM_NESTED_JAR_CLASS_PATH_OUTER`; interpreter round i2 wave 69,
/// lane L5; `i66-L5-proposal-reuse-the-class-path-parse-of-a-fat-jar-20261030-RETIRED-20261101.md`): the
/// `java -jar app.jar` shape, where the launcher's `jar:nested:` URLs name
/// the VM's own `-jar` archive. Its bytes (the class path's mapping, held for
/// the VM's life anyway) and a clone of its parsed central directory, so a
/// row built from it opens, maps, reads and parses nothing; `None` -- the
/// caller then builds the row as without the switch -- when the switch is
/// off, the fat jar is not an archive of the application class path, or the
/// file no longer has the stamp it had when the class path loaded it
/// (`ClassPath::outer_archive_for`). Consulted on a row BUILD only; the row
/// carries the class path's load-time stamp, so its re-check on every hit is
/// the one a row built by [`shared_outer_archive`] gets.
pub fn class_path_outer(
    ctx: &dyn NativeContext,
    outer: &str,
) -> Option<cratonvm_classloading::OuterArchive> {
    if !ctx.nested_jar_class_path_outer() {
        return None;
    }
    let lent = ctx.class_path_outer_archive(std::path::Path::new(outer))?;
    let lent = lent
        .downcast::<cratonvm_classloading::OuterArchive>()
        .ok()?;
    ctx.note_nested_jar_row(NestedJarRowEvent::ClassPathLent);
    Some(*lent)
}

/// The walk index memoised under `key` in `cache`, built by `build` on a miss
/// (outside the lock). A refusal is kept: the body runs, which is always
/// right. With `stamped`, a hit whose outer archive changed on disk is
/// rebuilt (and the row replaced) instead of answered; without it a hit is
/// answered as built. The map is cleared when it reaches `limit` rows.
/// [`memoised_walk_index_with`] without a refusal stamp.
#[cfg_attr(not(test), allow(dead_code))]
pub fn memoised_walk_index(
    cache: &OrderedPlMutex<WalkIndexCache>,
    key: &str,
    limit: usize,
    stamped: bool,
    build: impl FnOnce() -> Option<Arc<cratonvm_classloading::NestedEntryNameIndex>>,
) -> Option<Arc<cratonvm_classloading::NestedEntryNameIndex>> {
    memoised_walk_index_with(cache, key, limit, stamped, None, build)
}

/// [`memoised_walk_index`] with `refusal_path`
/// (`CRATONVM_NESTED_JAR_REFUSAL_STAMP`; interpreter round i2 wave 68, lane
/// L5): when `Some`, a refused row records the stamp of the file it names
/// (read BEFORE the refused build) and is rebuilt by a later ask that finds
/// the file changed ([`memoised_row`]).
#[cfg_attr(not(test), allow(dead_code))]
pub fn memoised_walk_index_with(
    cache: &OrderedPlMutex<WalkIndexCache>,
    key: &str,
    limit: usize,
    stamped: bool,
    refusal_path: Option<&dyn Fn() -> Option<std::path::PathBuf>>,
    build: impl FnOnce() -> Option<Arc<cratonvm_classloading::NestedEntryNameIndex>>,
) -> Option<Arc<cratonvm_classloading::NestedEntryNameIndex>> {
    memoised_walk_index_noted(cache, key, limit, stamped, refusal_path, &|_| {}, build)
}

/// [`memoised_walk_index_with`] reporting each row event to `note`
/// (interpreter round i2 wave 69, lane L5; the LOADER census).
pub fn memoised_walk_index_noted(
    cache: &OrderedPlMutex<WalkIndexCache>,
    key: &str,
    limit: usize,
    stamped: bool,
    refusal_path: Option<&dyn Fn() -> Option<std::path::PathBuf>>,
    note: &dyn Fn(NestedJarRowEvent),
    build: impl FnOnce() -> Option<Arc<cratonvm_classloading::NestedEntryNameIndex>>,
) -> Option<Arc<cratonvm_classloading::NestedEntryNameIndex>> {
    let on_changed = if stamped {
        ChangedRow::Rebuild
    } else {
        ChangedRow::Answer
    };
    memoised_row(
        cache,
        key,
        limit,
        on_changed,
        cratonvm_classloading::NestedEntryNameIndex::outer_unchanged,
        refusal_path,
        note,
        build,
    )
}

/// The bound of [`nested_jar_entry_index`]'s cache: one row per nested jar of
/// every Spring Boot fat jar the VM has walked.
pub const NESTED_JAR_INDEX_CACHE_LIMIT: usize = 4096;

/// One memoised walk index per `nested:<outer>/!<inner>!/` spec.
pub type WalkIndexCache = MemoCache<cratonvm_classloading::NestedEntryNameIndex>;

/// One memoised row per key: a built value, or a refusal.
pub type MemoCache<T> = std::collections::HashMap<String, MemoRow<T>>;

/// A row of a [`MemoCache`].
pub enum MemoRow<T> {
    /// Built: answered (after the caller's re-check, see [`ChangedRow`]).
    Built(Arc<T>),
    /// Refused: the body runs. `None` -- for the VM's life (the historical
    /// rule, and always for a key that names no file); `Some` -- until the
    /// file it names changes (`CRATONVM_NESTED_JAR_REFUSAL_STAMP`).
    Refused(Option<RefusalStamp>),
}

impl<T> Clone for MemoRow<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Built(row) => Self::Built(Arc::clone(row)),
            Self::Refused(stamp) => Self::Refused(stamp.clone()),
        }
    }
}

impl<T> MemoRow<T> {
    pub fn answer(self) -> Option<Arc<T>> {
        match self {
            Self::Built(row) => Some(row),
            Self::Refused(_) => None,
        }
    }
}

/// What a refused row remembers under `CRATONVM_NESTED_JAR_REFUSAL_STAMP`
/// (interpreter round i2 wave 68, lane L5;
/// `i67-L5-a-nested-jar-index-refusal-is-remembered-for-the-vm-life`): the
/// file the refused build read, its stamp read BEFORE that build (`None`:
/// there was no such file), and when the refusal was recorded.
#[derive(Clone)]
pub struct RefusalStamp {
    pub path: std::path::PathBuf,
    pub outer: Option<cratonvm_classloading::OuterFileStamp>,
    pub at: std::time::Instant,
}

impl RefusalStamp {
    /// May the refused row be rebuilt now: at least
    /// [`NESTED_JAR_REFUSAL_RETRY_INTERVAL`] after the refusal (no `stat`
    /// before that, so a refused row costs nothing for that long), and the
    /// file's stamp differs from the one the refused build started from (one
    /// `stat`). A permanently unreadable jar keeps its stamp and stays
    /// refused; a jar rewritten continuously is rebuilt at most once per
    /// interval.
    pub fn may_retry(&self) -> bool {
        self.at.elapsed() >= NESTED_JAR_REFUSAL_RETRY_INTERVAL
            && cratonvm_classloading::OuterFileStamp::read(&self.path) != self.outer
    }
}

/// The least time between two rebuilds of one refused row
/// ([`RefusalStamp::may_retry`]).
pub const NESTED_JAR_REFUSAL_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(100);

/// What [`memoised_row`] does with a built row whose file changed on disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChangedRow {
    /// Answer it without looking (no `stat`): the unstamped walk index.
    Answer,
    /// Rebuild it and replace the row: the stamped walk index; either bytes
    /// index under `CRATONVM_NESTED_JAR_REFUSAL_STAMP`.
    Rebuild,
    /// Turn it into a refusal for the VM's life: the bytes indexes before.
    Refuse,
}

/// The row memoised under `key` in `cache`, built by `build` on a miss
/// (outside the lock; racing builds keep the first insert). A built hit is
/// re-checked with `unchanged` (one `stat`, outside the lock) unless
/// `on_changed` is [`ChangedRow::Answer`], and a changed one is handled as
/// `on_changed` says. A refusal answers `None` -- for the VM's life when
/// `refusal_path` is `None` (or names no file); otherwise (wave 68,
/// `CRATONVM_NESTED_JAR_REFUSAL_STAMP`) it records the stamp of
/// `refusal_path()` read BEFORE the refused build, and a later ask rebuilds
/// it once [`RefusalStamp::may_retry`] holds. The map is cleared when it
/// reaches `limit` rows. Each row event is reported to `note` (wave 69, the
/// LOADER census's `nested-jar-rows-*` counters; diagnostic only).
#[allow(clippy::too_many_arguments)]
pub fn memoised_row<T>(
    cache: &OrderedPlMutex<MemoCache<T>>,
    key: &str,
    limit: usize,
    on_changed: ChangedRow,
    unchanged: impl Fn(&T) -> bool,
    refusal_path: Option<&dyn Fn() -> Option<std::path::PathBuf>>,
    note: &dyn Fn(NestedJarRowEvent),
    build: impl FnOnce() -> Option<Arc<T>>,
) -> Option<Arc<T>> {
    let hit = cache.lock().get(key).cloned();
    match hit {
        Some(MemoRow::Built(row)) => {
            if on_changed == ChangedRow::Answer || unchanged(&row) {
                return Some(row);
            }
            if on_changed == ChangedRow::Refuse {
                cache.lock().insert(key.to_string(), MemoRow::Refused(None));
                note(NestedJarRowEvent::Refused);
                return None;
            }
            note(NestedJarRowEvent::Rebuilt);
            return replace_row(cache, key, limit, refusal_path, note, build);
        }
        Some(MemoRow::Refused(None)) => {
            note(NestedJarRowEvent::RefusalKept);
            return None;
        }
        Some(MemoRow::Refused(Some(refused))) => {
            if !refused.may_retry() {
                note(NestedJarRowEvent::RefusalKept);
                return None;
            }
            note(NestedJarRowEvent::RefusalRetried);
            return replace_row(cache, key, limit, refusal_path, note, build);
        }
        None => {}
    }
    let built = build_row(refusal_path, build);
    note(match built {
        MemoRow::Built(_) => NestedJarRowEvent::Built,
        MemoRow::Refused(_) => NestedJarRowEvent::Refused,
    });
    let mut guard = cache.lock();
    if guard.len() >= limit {
        guard.clear();
    }
    guard
        .entry(key.to_string())
        .or_insert(built)
        .clone()
        .answer()
}

/// [`memoised_row`]'s rebuild: the new row replaces whatever is there.
pub fn replace_row<T>(
    cache: &OrderedPlMutex<MemoCache<T>>,
    key: &str,
    limit: usize,
    refusal_path: Option<&dyn Fn() -> Option<std::path::PathBuf>>,
    note: &dyn Fn(NestedJarRowEvent),
    build: impl FnOnce() -> Option<Arc<T>>,
) -> Option<Arc<T>> {
    let rebuilt = build_row(refusal_path, build);
    if matches!(rebuilt, MemoRow::Refused(_)) {
        note(NestedJarRowEvent::Refused);
    }
    let mut guard = cache.lock();
    if guard.len() >= limit {
        guard.clear();
    }
    guard.insert(key.to_string(), rebuilt.clone());
    rebuilt.answer()
}

/// `build()` as a row: a refusal carries the stamp of `refusal_path()` read
/// BEFORE the build, so a rewrite racing the build leaves a stamp that no
/// longer matches the file (the next ask past the interval retries).
pub fn build_row<T>(
    refusal_path: Option<&dyn Fn() -> Option<std::path::PathBuf>>,
    build: impl FnOnce() -> Option<Arc<T>>,
) -> MemoRow<T> {
    let before = refusal_path.and_then(|path| path()).map(|path| {
        let outer = cratonvm_classloading::OuterFileStamp::read(&path);
        (path, outer)
    });
    match build() {
        Some(row) => MemoRow::Built(row),
        None => MemoRow::Refused(before.map(|(path, outer)| RefusalStamp {
            path,
            outer,
            at: std::time::Instant::now(),
        })),
    }
}

// ---------------------------------------------------------------------------
// The nested-jar bytes index (interpreter round i2 wave 63, lane L5;
// `docs/feature-designs/native-dispatch-architecture.md`).
// ---------------------------------------------------------------------------

/// The JDK's `Resource`, whose concrete `getBytes()` the intrinsic stands on.
pub const JDK_LOADER_RESOURCE: &str = "jdk/internal/loader/Resource";

/// The anonymous `Resource` the generic `URLClassPath$Loader.getResource`
/// returns (JDK 25: the only anonymous class of `Loader`), capturing
/// `val$name`, `val$url`, `val$uc` and `this$0`.
pub const URLCLASSPATH_LOADER_RESOURCE: &str = "jdk/internal/loader/URLClassPath$Loader$1";

/// What the bytes intrinsic found for one receiver.
pub enum NestedJarResourceBytes {
    /// Not the generic loader's `Resource`: the JDK body, uncounted.
    NotLoaderResource,
    /// The generic loader's `Resource`, but some gate failed: the JDK body.
    Body,
    /// Every gate held: `bytes` are the entry's, `stream` the `Resource`'s
    /// cached input stream (`cis`), already open.
    Serve { bytes: Vec<u8>, stream: ObjectRef },
}

/// Reviewed `cratonvm_native_api::NativeKind::Intrinsic` on `jdk/internal/loader/Resource.getBytes()[B`
/// (`CRATONVM_NESTED_JAR_BYTES_INDEX`, default off; registered where the
/// `URLClassLoader` family runs the JDK's bodies). For the generic
/// `URLClassPath$Loader`'s `Resource` over a Spring Boot `jar:nested:` URL it
/// answers the entry's bytes from the VM's own copy of the nested jar
/// ([`nested_jar_resource_bytes`] lists the gates) instead of the body's read
/// loop over Spring's stream stack (a `JarUrlConnection` input stream over a
/// `NestedJarFile` inflater, interpreted); every other receiver, and any
/// doubt, runs the JDK body.
///
/// What the served path keeps of the JDK 25 body
/// (`InputStream in = cachedInputStream(); try { int len = getContentLength();
/// ... in.read ... } finally { try { in.close(); } catch (IOException ignore) {} }`):
///
/// * `cachedInputStream()` -- served only when `cis` is already set (the
///   `URLClassLoader.defineClass` path's `getByteBuffer()` set it), so the
///   call would only have returned it;
/// * `getContentLength()` is called (virtually, as the body does); if it
///   throws, the stream is closed and the exception propagates;
/// * the length rule: `-1` or `Integer.MAX_VALUE` reads to the end (the
///   entry's bytes); another negative length reads nothing (`new byte[0]`);
///   a length up to the entry's size reads that prefix; a larger one hits
///   end of stream first, so the body throws `EOFException("Detect premature
///   EOF")` after closing;
/// * `in.close()` is called; an `IOException` from it is ignored, anything
///   else propagates, as from the body's `finally`.
///
/// What it skips is the `in.read(b, pos, n)` calls on the stream it then
/// closes: Spring's internal inflation of the same bytes.
pub fn nested_jar_resource_get_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    resource_get_bytes(ctx, args, true, false)
}

/// The served path of [`nested_jar_resource_get_bytes`]: the body's calls
/// on `this` and `stream` in the body's order, with the bytes from Rust.
pub fn serve_nested_jar_resource_bytes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    stream: ObjectRef,
    bytes: Vec<u8>,
) -> MethodCallResult {
    // GC-safety: `stream` (and below the result array) live across calls
    // into Java and an allocation.
    let base = ctx.pin_native_root(stream);
    let result = serve_nested_jar_resource_bytes_pinned(ctx, this, stream, base, &bytes);
    ctx.unpin_native_roots(base);
    result
}

pub fn serve_nested_jar_resource_bytes_pinned(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    stream: ObjectRef,
    stream_pin: usize,
    bytes: &[u8],
) -> MethodCallResult {
    let len = match ctx.invoke_virtual(this, "getContentLength", "()I", &[]) {
        Ok(Some(Value::Int(len))) => len,
        Ok(_) => -1,
        Err(failed) => {
            let stream = ctx.read_native_pin(stream_pin, stream);
            return match close_resource_stream(ctx, stream) {
                Err(replaced) => Err(replaced),
                Ok(()) => Err(failed),
            };
        }
    };
    let (served, premature_eof) = nested_jar_resource_served_len(len, bytes.len());
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, served);
    if served > 0 && !ctx.write_byte_array_from(arr, 0, &bytes[..served]) {
        // Cannot happen for a fresh array of that length; never answer
        // wrong bytes if it does.
        return Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Internal {
                message: "nested-jar bytes: byte[] copy failed".to_string(),
            },
        ));
    }
    let arr_pin = ctx.pin_native_root(arr);
    let stream = ctx.read_native_pin(stream_pin, stream);
    close_resource_stream(ctx, stream)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    if premature_eof {
        let message = ctx.create_string("Detect premature EOF");
        return Err(cratonvm_native_api::throwable_builder::throw_new(
            ctx,
            "java/io/EOFException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(message))],
            || {
                MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Internal {
                        message: "nested-jar bytes: EOFException not constructed".to_string(),
                    },
                )
            },
        ));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `Resource.getBytes`'s length rule over a stream of exactly `available`
/// bytes: how many bytes the body returns, and whether it throws
/// `EOFException` (after closing) instead.
pub fn nested_jar_resource_served_len(len: i32, available: usize) -> (usize, bool) {
    if len == -1 || len == i32::MAX {
        return (available, false);
    }
    if len < 0 {
        return (0, false);
    }
    let len = len as usize;
    if len <= available {
        (len, false)
    } else {
        (available, true)
    }
}

/// `try { in.close(); } catch (IOException ignore) {}`.
pub fn close_resource_stream(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
) -> Result<(), MethodCallFailed> {
    match ctx.invoke_virtual(stream, "close", "()V", &[]) {
        Ok(_) => Ok(()),
        Err(MethodCallFailed::ExceptionThrown(exc))
            if object_extends(&*ctx, exc, "java/io/IOException") =>
        {
            Ok(())
        }
        Err(failed) => Err(failed),
    }
}

/// The gates of [`nested_jar_resource_get_bytes`]; every one must hold to
/// serve, any doubt runs the body:
///
/// * the receiver is exactly `URLClassPath$Loader$1` (the generic loader's
///   `Resource`; `JarLoader` / `FileLoader` resources have their own
///   classes);
/// * its `val$url` carries Spring Boot's `jar:` handler and no `ref`, and its
///   `file` is `nested:<outer>/!<inner>!/<name>` with the `<inner>` the walk
///   index accepts ([`nested_jar_spec_parts`]) and a plain `<name>`
///   ([`nested_jar_walk_name_is_plain`]: the URL names exactly that entry);
/// * the `Resource`'s `cis` is already set (see the intrinsic);
/// * the VM's copy of the nested jar ([`nested_jar_archive`]) has the plain
///   view (not Multi-Release, unsigned), the outer file is unchanged since it
///   was opened, and holds `<name>` as a file entry whose bytes read cleanly
///   (`NestedArchiveEntries::entry_bytes`).
pub fn nested_jar_resource_bytes(ctx: &dyn NativeContext, args: &[Value]) -> NestedJarResourceBytes {
    let Some(Value::Object(Some(this))) = args.first() else {
        return NestedJarResourceBytes::NotLoaderResource;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(*this))
        .as_deref()
        != Some(URLCLASSPATH_LOADER_RESOURCE)
    {
        return NestedJarResourceBytes::NotLoaderResource;
    }
    let Value::Object(Some(url)) = ctx.get_field_by_name(*this, "val$url") else {
        return NestedJarResourceBytes::Body;
    };
    let Some(file) = spring_nested_url_file(ctx, url) else {
        return NestedJarResourceBytes::Body;
    };
    let Some((spec, name)) = nested_jar_url_entry_parts(&file) else {
        return NestedJarResourceBytes::Body;
    };
    if !nested_jar_walk_name_is_plain(name) {
        return NestedJarResourceBytes::Body;
    }
    let Value::Object(Some(stream)) = ctx.get_field_by_name(*this, "cis") else {
        return NestedJarResourceBytes::Body;
    };
    let Some(archive) = nested_jar_archive(ctx, spec) else {
        return NestedJarResourceBytes::Body;
    };
    // `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE` (wave 69): a directory-only
    // row's read inside a burst reuses THIS VM's kept outer handle
    // (`cratonvm_classloading::OuterHandlePool`); the same answer either way.
    let pool = archive_bytes_caches(ctx)
        .filter(|_| ctx.nested_jar_outer_handle_reuse())
        .map(|caches| &caches.outer_handles);
    let (bytes, reused) = archive.entry_bytes_pooled_noting(name, pool);
    if reused {
        ctx.note_nested_jar_row(NestedJarRowEvent::OuterHandleReused);
    }
    match bytes {
        Some(bytes) => NestedJarResourceBytes::Serve { bytes, stream },
        None => NestedJarResourceBytes::Body,
    }
}

/// The `file` of a URL whose handler is Spring Boot's `jar:` handler and
/// whose `ref` is null; `None` otherwise.
pub fn spring_nested_url_file(ctx: &dyn NativeContext, url: ObjectRef) -> Option<String> {
    let Value::Object(Some(handler)) = ctx.get_field_by_name(url, "handler") else {
        return None;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(handler))
        .as_deref()
        != Some(SPRING_NESTED_JAR_HANDLER)
    {
        return None;
    }
    if !matches!(ctx.get_field_by_name(url, "ref"), Value::Object(None)) {
        return None;
    }
    match ctx.get_field_by_name(url, "file") {
        Value::Object(Some(file)) => ctx.read_string(file),
        _ => None,
    }
}

/// `nested:<outer>/!<inner>!/<name>` -> (`nested:<outer>/!<inner>!/`,
/// `<name>`): the spec of the nested jar (as a walk-index loader's base URL
/// spells it) and the entry name. `None` when the shape is not that, the
/// spec is one [`nested_jar_spec_parts`] refuses, or `<name>` is empty.
pub fn nested_jar_url_entry_parts(file: &str) -> Option<(&str, &str)> {
    let separator = file.find("/!")?;
    let inner_start = separator + 2;
    let entry_start = inner_start + file[inner_start..].find("!/")? + 2;
    let (spec, name) = file.split_at(entry_start);
    if name.is_empty() {
        return None;
    }
    nested_jar_spec_parts(spec)?;
    Some((spec, name))
}

/// The VM's copy of the nested jar a `nested:<outer>/!<inner>!/` spec names
/// (`ClassPath::open_nested_archive`), memoised by the spec in THIS VM's
/// [`ArchiveBytesCaches`] (a pure function of a file on disk, no Java object;
/// wave 63 kept it in a process global, moved per VM in wave 64). `None`
/// (memoised too) when the spec is not that shape or the jar cannot be read,
/// and from the first ask that finds the outer file changed on disk: from
/// then on the body reads that jar. Under `CRATONVM_NESTED_JAR_REFUSAL_STAMP`
/// (wave 68) a changed file rebuilds the row instead, and a refusal is
/// retried once the outer file changes ([`memoised_archive_with`]).
pub fn nested_jar_archive(
    ctx: &dyn NativeContext,
    spec: &str,
) -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>> {
    let caches = archive_bytes_caches(ctx);
    // `CRATONVM_NESTED_JAR_SHARED_OUTER` (wave 66): see [`shared_outer_archive`].
    let shared = caches.filter(|_| ctx.nested_jar_shared_outer());
    // `CRATONVM_NESTED_JAR_OUTER_INDEX` (wave 67): see [`shared_outer_index`];
    // the row then owns a copy of the nested jar, no outer mapping.
    let indexed = caches.filter(|_| ctx.nested_jar_outer_index());
    // `CRATONVM_NESTED_JAR_BYTES_DIRECTORY` (wave 68): over the outer index,
    // a stored nested jar's row keeps its directory only and reads each entry
    // from the outer file at the ask
    // (`ClassPath::open_nested_archive_directory_in_index`).
    let directory = ctx.nested_jar_bytes_directory();
    let open = || -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>> {
        let (outer, inner) = nested_jar_spec_parts(spec)?;
        // `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` (wave 69): a fat jar on the
        // VM's application class path lends its own bytes and parse; a stored
        // nested jar is then a range of the class path's own mapping
        // ([`class_path_outer`]).
        if let Some(outer) = class_path_outer(ctx, &outer) {
            return cratonvm_classloading::ClassPath::open_nested_archive_in(&outer, &inner)
                .map(Arc::new);
        }
        match (indexed, shared) {
            (Some(caches), _) => {
                let outer = shared_outer_index(caches, &outer)?;
                if directory {
                    cratonvm_classloading::ClassPath::open_nested_archive_directory_in_index(
                        &outer, &inner,
                    )
                    .map(Arc::new)
                } else {
                    cratonvm_classloading::ClassPath::open_nested_archive_in_index(&outer, &inner)
                        .map(Arc::new)
                }
            }
            (None, Some(caches)) => {
                let outer = shared_outer_archive(caches, &outer)?;
                cratonvm_classloading::ClassPath::open_nested_archive_in(&outer, &inner)
                    .map(Arc::new)
            }
            (None, None) => {
                let outer = std::path::Path::new(&outer);
                let map = caches.is_some_and(|caches| caches.on_vm_class_path(ctx, outer));
                cratonvm_classloading::ClassPath::open_nested_archive(outer, &inner, map)
                    .map(Arc::new)
            }
        }
    };
    // `CRATONVM_NESTED_JAR_REFUSAL_STAMP` (wave 68): see [`memoised_archive_with`].
    let outer_path =
        || nested_jar_spec_parts(spec).map(|(outer, _)| std::path::PathBuf::from(outer));
    let refusal_path = ctx
        .nested_jar_refusal_stamp()
        .then_some(&outer_path as &dyn Fn() -> Option<std::path::PathBuf>);
    let note = |event| ctx.note_nested_jar_row(event);
    // `CRATONVM_NESTED_JAR_POOLED_STAT` (interpreter round i3 wave 71): a
    // directory-only row whose fat jar has a kept handle in THIS VM's pool
    // skips the hit's `stat`, because the pooled read that follows checks the
    // file itself (`NestedArchiveEntries::pooled_read_checks_outer`); a
    // changed file is then answered by the body once and the row rebuilt on
    // the next ask, as before.
    let pooled = caches
        .filter(|_| ctx.nested_jar_pooled_stat() && ctx.nested_jar_outer_handle_reuse())
        .map(|caches| &caches.outer_handles);
    let unchanged = |row: &cratonvm_classloading::NestedArchiveEntries| {
        if pooled.is_some_and(|pool| row.pooled_read_checks_outer(pool)) {
            ctx.note_nested_jar_row(NestedJarRowEvent::PooledStatSkipped);
            return true;
        }
        row.outer_unchanged()
    };
    match caches {
        Some(caches) => memoised_archive_checked(
            &caches.nested,
            spec,
            NESTED_JAR_ARCHIVE_CACHE_LIMIT,
            refusal_path,
            &note,
            unchanged,
            open,
        ),
        // No per-VM slot (mock contexts): open for this ask only.
        None => open().filter(|archive| archive.outer_unchanged()),
    }
}

/// The bound of [`nested_jar_archive`]'s cache: each row holds the outer
/// archive's mapping (shared) and, for a deflated nested jar, its inflated
/// copy -- or, for a fat jar off the VM's class path, a copy of the nested
/// jar ([`ArchiveBytesCaches::on_vm_class_path`]).
pub const NESTED_JAR_ARCHIVE_CACHE_LIMIT: usize = 1024;

/// The bound of [`jar_loader_archive`]'s cache: each row holds one jar's
/// mapping (a copy for a jar off the VM's class path, see
/// [`ArchiveBytesCaches::on_vm_class_path`]) and its entry-name index.
pub const JAR_LOADER_ARCHIVE_CACHE_LIMIT: usize = 4096;

/// One memoised archive per key (a nested spec, or a jar's path); refused
/// rows remember a refusal ([`MemoRow`]).
pub type ArchiveBytesCache = MemoCache<cratonvm_classloading::NestedArchiveEntries>;

/// The `Resource.getBytes` intrinsics' archive caches of ONE VM, kept in the
/// VM's native registry (`NativeContext::archive_bytes_cache_slot`), not in a
/// process global (AGENTS.md: no process globals for compatibility state).
/// What they hold is a byte cache of files on disk, keyed by path, with the
/// file's length and mtime re-checked on every ask; two VMs over the same jar
/// each map it once. Interpreter round i2 wave 64, lane L5. Wave 65 added
/// the `URLClassPath$Loader` walk index's entry-name sets (`walk`), which
/// re-check the file only under `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP`.
pub struct ArchiveBytesCaches {
    /// `CRATONVM_NESTED_JAR_BYTES_INDEX`: by `nested:<outer>/!<inner>!/` spec.
    pub nested: OrderedPlMutex<ArchiveBytesCache>,
    /// `CRATONVM_JAR_LOADER_BYTES_INDEX`: by the `ZipFile`'s path.
    pub jar: OrderedPlMutex<ArchiveBytesCache>,
    /// `CRATONVM_NESTED_JAR_WALK_INDEX`'s entry-name sets, by
    /// `nested:<outer>/!<inner>!/` spec ([`nested_jar_entry_index`]; a
    /// process global until wave 65).
    pub walk: OrderedPlMutex<WalkIndexCache>,
    /// The `URLClassLoader` local class paths of [`cached_class_path_for_paths`],
    /// by the exact paths vector (a process global until wave 65).
    pub url_class_path: OrderedPlMutex<UrlClassPathCache>,
    /// `CRATONVM_NESTED_JAR_SHARED_OUTER`: one open outer fat jar per path,
    /// shared by the `nested` and `walk` rows built from it
    /// ([`shared_outer_archive`]; interpreter round i2 wave 66, lane L5).
    pub outer: OrderedPlMutex<OuterArchiveCache>,
    /// `CRATONVM_NESTED_JAR_OUTER_INDEX`: one parsed outer central directory
    /// (no bytes) per path, shared by the `nested` and `walk` rows built from
    /// it ([`shared_outer_index`]; interpreter round i2 wave 67, lane L5).
    pub outer_index: OrderedPlMutex<OuterIndexCache>,
    /// `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`: the outer fat-jar handles
    /// kept open for the length of a burst of directory-only row reads (at
    /// most one per fat jar, four fat jars; closed by the pool's reaper about
    /// 50 ms after a burst's last read, and when this cache is dropped;
    /// interpreter round i2 wave 69, lane L5).
    pub outer_handles: cratonvm_classloading::OuterHandlePool,
    /// The VM's own class path archives (`java.class.path` when first
    /// asked), canonicalised: the only files these caches may MAP. See
    /// [`Self::on_vm_class_path`].
    pub class_path: OnceLock<std::collections::HashSet<std::path::PathBuf>>,
    /// `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY`: the supertype names each
    /// defining loader's `loadClass` already answered for the define natives'
    /// pre-pass ([`supertype_link_known`]; interpreter round i3 wave 72, lane
    /// L5).
    pub supertype_links: OrderedPlMutex<SupertypeLinks>,
    /// `CRATONVM_LOADER_KIND_MEMO`: the loader kind of each `ClassLoader`
    /// subclass a loader predicate was asked about ([`memoized_loader_kind`];
    /// interpreter round i3 wave 81, lane L5).
    pub loader_kinds: cratonvm_types::lock_order::OrderedPlRwLock<LoaderKinds>,
    /// `CRATONVM_LOADER_NAMESPACE_INDEX`: each user loader's namespace row for
    /// the current GC epoch, and the classes found on the real `ClassLoader`
    /// layout ([`LoaderNamespaceIndex`]; interpreter round i3 wave 83, lane
    /// L5).
    pub loader_namespaces: cratonvm_types::lock_order::OrderedPlRwLock<LoaderNamespaceIndex>,
    /// `Class.getPackage()`'s parsed jar / exploded-archive manifests
    /// (`lang_class::cached_package_manifest`), by the manifest's source key,
    /// each row stamped with its file's length and modification time and
    /// dropped when the stamp moves. Three process globals keyed by path only
    /// until interpreter round i4 wave 99, lane L5
    /// (`i98-orch-manifest-caches-are-process-wide-and-never-invalidated`).
    pub package_manifests: OrderedPlMutex<PackageManifestCache>,
    /// `CRATONVM_DEFINE_PD_URL_MEMO`: the code-source URL of each
    /// `ProtectionDomain` a `defineClass*` native decoded, for the current GC
    /// address epoch ([`PdUrlMemo`]; interpreter round i6 wave 120, lane L5).
    pub pd_urls: cratonvm_types::lock_order::OrderedPlRwLock<PdUrlMemo>,
    /// `CRATONVM_BUILTIN_FOR_NAME_RECORD`: the class each built-in loader
    /// (application, platform) answered a `Class.forName` with, by name
    /// ([`crate::lang_class::BuiltinForNameRecord`]; interpreter round i6 wave
    /// 133, lane L5d).
    pub builtin_for_name: BuiltinForNameLock,
}

/// [`ArchiveBytesCaches::builtin_for_name`]'s lock.
pub type BuiltinForNameLock =
    cratonvm_types::lock_order::OrderedPlRwLock<crate::lang_class::BuiltinForNameRecord>;

/// THIS VM's `Class.forName` record of the built-in loaders
/// (`CRATONVM_BUILTIN_FOR_NAME_RECORD`); `None` when the context has no
/// per-VM slot (mock contexts), which then ask the loader every time.
/// Interpreter round i6 wave 133, lane L5d.
pub fn builtin_for_name_record(ctx: &dyn NativeContext) -> Option<&BuiltinForNameLock> {
    archive_bytes_caches(ctx).map(|caches| &caches.builtin_for_name)
}

/// `CRATONVM_DEFINE_PD_URL_MEMO` (interpreter round i6 wave 120, lane L5;
/// umbrella `i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot`,
/// "Wave 120"): `ProtectionDomain` ADDRESS -> the code-source URL
/// [`extract_pd_code_source_url`] decoded from it (`None`: it had none). A
/// `URLClassLoader` hands every class of one jar the SAME domain
/// (`SecureClassLoader.pdcache`), and a domain's `codesource`, its
/// `location` and the URL's fields never change after construction, so the
/// decode -- by-name field reads and Java `String` decodes of the URL's
/// protocol, authority, file and ref -- is the same answer for every class of
/// the jar. A row belongs to one GC address epoch (`gc_address_epoch`): no
/// object moves or dies between two moves of it, so an address names one
/// domain; the first ask of a later epoch clears the rows, and a filing is
/// refused if the epoch moved while the decode ran.
#[derive(Default)]
pub struct PdUrlMemo {
    pub epoch: u64,
    // addr-keyed: valid for ONE GC address epoch only -- cleared on the first ask or filing after the epoch moves.
    pub rows: rustc_hash::FxHashMap<usize, Option<Arc<str>>>,
}

impl PdUrlMemo {
    /// Rows kept before the table is cleared.
    pub const CAP: usize = 4096;

    pub fn get(&self, epoch: u64, addr: usize) -> Option<Option<Arc<str>>> {
        if self.epoch != epoch {
            return None;
        }
        self.rows.get(&addr).cloned()
    }

    pub fn insert(&mut self, epoch: u64, addr: usize, url: Option<Arc<str>>) {
        if self.epoch != epoch {
            self.rows.clear();
            self.epoch = epoch;
        }
        if self.rows.len() >= Self::CAP && !self.rows.contains_key(&addr) {
            self.rows.clear();
        }
        self.rows.insert(addr, url);
    }
}

/// [`extract_pd_code_source_url`] for the `defineClass*` natives, through
/// [`PdUrlMemo`] under `CRATONVM_DEFINE_PD_URL_MEMO` (default on, `=0` off;
/// `LoaderFlags::define_pd_url_memo`). The same answer; with the switch off,
/// or with no per-VM slot (mock contexts), the decode itself.
pub fn extract_pd_code_source_url_for_define(
    ctx: &dyn NativeContext,
    pd: ObjectRef,
) -> Option<String> {
    if !cratonvm_types::flags().loader.define_pd_url_memo {
        return extract_pd_code_source_url(ctx, pd);
    }
    let Some(caches) = archive_bytes_caches(ctx) else {
        return extract_pd_code_source_url(ctx, pd);
    };
    let epoch = ctx.gc_address_epoch();
    let addr = pd.as_ptr() as usize;
    if let Some(hit) = caches.pd_urls.read().get(epoch, addr) {
        return hit.map(|url| url.to_string());
    }
    let url = extract_pd_code_source_url(ctx, pd);
    if ctx.gc_address_epoch() == epoch {
        caches
            .pd_urls
            .write()
            .insert(epoch, addr, url.as_deref().map(Arc::from));
    }
    url
}

/// A file's stamp: its length and modification time (`None` when the
/// platform reports none). A row whose file no longer has this stamp is
/// stale.
pub type FileStamp = (u64, Option<std::time::SystemTime>);

/// One VM's `Class.getPackage()` manifests: source key -> (the stamp of the
/// file the row was parsed from, the parsed main + per-package attributes).
pub type PackageManifestCache =
    std::collections::HashMap<String, (FileStamp, ParsedPackageManifest)>;

/// One parsed manifest: main and per-package attributes
/// (`lang_class::parse_package_manifest`).
pub type ParsedPackageManifest = Arc<std::collections::HashMap<String, String>>;

/// The bound of [`ArchiveBytesCaches::package_manifests`]: cleared when full
/// (a row is a few attributes of one jar; a process rarely holds more).
pub const PACKAGE_MANIFEST_CACHE_LIMIT: usize = 4096;

/// THIS VM's `Class.getPackage()` manifest cache; `None` when the context has
/// no per-VM slot (mock contexts), which then parse for the ask only. Dropped
/// with the VM's native registry, as every [`ArchiveBytesCaches`] row is.
pub fn package_manifest_cache(
    ctx: &dyn NativeContext,
) -> Option<&OrderedPlMutex<PackageManifestCache>> {
    archive_bytes_caches(ctx).map(|caches| &caches.package_manifests)
}

impl ArchiveBytesCaches {
    pub fn new() -> Self {
        // `Scratch` (L0): every critical section is a map probe, an insert or
        // a clear; no two of the maps are ever locked together. Archives are
        // opened, stat'ed and read outside them.
        Self {
            nested: OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch),
            jar: OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch),
            walk: OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch),
            url_class_path: new_url_class_path_cache(),
            outer: OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch),
            outer_index: OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch),
            outer_handles: cratonvm_classloading::OuterHandlePool::new(),
            class_path: OnceLock::new(),
            supertype_links: OrderedPlMutex::new(SupertypeLinks::default(), LockLevel::Scratch),
            loader_kinds: cratonvm_types::lock_order::OrderedPlRwLock::new(
                LoaderKinds::default(),
                LockLevel::Scratch,
            ),
            loader_namespaces: cratonvm_types::lock_order::OrderedPlRwLock::new(
                LoaderNamespaceIndex::default(),
                LockLevel::Scratch,
            ),
            package_manifests: OrderedPlMutex::new(
                std::collections::HashMap::new(),
                LockLevel::Scratch,
            ),
            pd_urls: cratonvm_types::lock_order::OrderedPlRwLock::new(
                PdUrlMemo::default(),
                LockLevel::Scratch,
            ),
            builtin_for_name: cratonvm_types::lock_order::OrderedPlRwLock::new(
                crate::lang_class::BuiltinForNameRecord::default(),
                LockLevel::Scratch,
            ),
        }
    }

    /// May the archive at `path` be memory-mapped: is it one of the VM's own
    /// class path archives (`-cp` / `-jar`, which the class path itself
    /// already maps on the assumption that they are immutable while the VM
    /// runs)? A jar a `URLClassLoader` was handed is read into memory
    /// instead: the application may truncate or rewrite it in place while a
    /// row still holds it, and a mapping turns that into `SIGBUS` where
    /// HotSpot's `ZipFile` throws or serves the old bytes. Interpreter round
    /// i2 wave 64, lane L5 follow-up
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub fn on_vm_class_path(&self, ctx: &dyn NativeContext, path: &std::path::Path) -> bool {
        let class_path = self.class_path.get_or_init(|| {
            ctx.get_system_property("java.class.path")
                .map(|cp| {
                    std::env::split_paths(&cp)
                        .filter(|entry| !entry.as_os_str().is_empty())
                        .filter_map(|entry| std::fs::canonicalize(entry).ok())
                        .collect()
                })
                .unwrap_or_default()
        });
        !class_path.is_empty()
            && std::fs::canonicalize(path).is_ok_and(|path| class_path.contains(&path))
    }
}

/// THIS VM's [`ArchiveBytesCaches`], created on first use; `None` when the
/// context has no per-VM slot (mock contexts).
pub fn archive_bytes_caches(ctx: &dyn NativeContext) -> Option<&ArchiveBytesCaches> {
    let slot = ctx.archive_bytes_cache_slot()?;
    let boxed = slot.get_or_init(|| -> Box<dyn std::any::Any + Send + Sync> {
        Box::new(ArchiveBytesCaches::new())
    });
    let any: &(dyn std::any::Any + Send + Sync) = &**boxed;
    any.downcast_ref::<ArchiveBytesCaches>()
}

/// `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY` (interpreter round i3 wave 72, lane
/// L5): per user-loader namespace id, the supertype names (internal form)
/// whose `loadClass` the define natives' pre-pass already ran successfully on
/// that loader -- the rows HotSpot's `SystemDictionary::resolve_super_or_fail`
/// finds in the loader's dictionary (the loader defined the name, or
/// `load_instance_class` recorded it as the name's initiating loader), so it
/// asks the loader's `loadClass` for a supertype once per (loader, name)
/// rather than once per define.
///
/// A namespace id is never reused, so a row of a dead loader is only garbage;
/// the table is cleared at [`Self::CAP`] rows (a cleared row costs one more
/// `loadClass`, the behaviour without the switch).
///
/// Each row keeps the class the loader answered (interpreter round i4 wave
/// 96, lane L5): a define that skips the ask hands that class to the define
/// as its supertype override, as the define that asked did. Before, the
/// skipped define linked the name through the flat store, which cannot see a
/// class the loader delegates to outside its parent chain -- a module layer's
/// `jdk.internal.loader.Loader` answering another module's class through
/// `remotePackageToLoader` -- so the loader's second class implementing
/// `api.Svc` failed with `NoClassDefFoundError: api/Svc`
/// (`L5W86ModuleLayers`, `many-loaders`).
#[derive(Default)]
pub struct SupertypeLinks {
    pub by_loader: std::collections::HashMap<
        u32,
        std::collections::HashMap<Box<str>, Option<cratonvm_types::ClassId>>,
    >,
    pub rows: usize,
}

impl SupertypeLinks {
    /// Rows kept before the table is cleared.
    pub const CAP: usize = 1 << 16;

    /// Did `loader_ns`'s `loadClass` already answer `internal` for a define?
    pub fn contains(&self, loader_ns: u32, internal: &str) -> bool {
        self.by_loader
            .get(&loader_ns)
            .is_some_and(|names| names.contains_key(internal))
    }

    /// The class `loader_ns`'s `loadClass` answered for `internal`, when a
    /// row records one.
    pub fn answer(&self, loader_ns: u32, internal: &str) -> Option<cratonvm_types::ClassId> {
        self.by_loader
            .get(&loader_ns)
            .and_then(|names| names.get(internal).copied().flatten())
    }

    /// Record that `loader_ns`'s `loadClass` answered `internal`.
    pub fn insert(&mut self, loader_ns: u32, internal: &str) {
        self.insert_answer(loader_ns, internal, None);
    }

    /// Record that `loader_ns`'s `loadClass` answered `internal` with
    /// `answer` (`None`: identity unknown). A repeat replaces the identity.
    pub fn insert_answer(
        &mut self,
        loader_ns: u32,
        internal: &str,
        answer: Option<cratonvm_types::ClassId>,
    ) {
        if let Some(cell) = self
            .by_loader
            .get_mut(&loader_ns)
            .and_then(|names| names.get_mut(internal))
        {
            if answer.is_some() {
                *cell = answer;
            }
            return;
        }
        if self.rows >= Self::CAP {
            self.by_loader.clear();
            self.rows = 0;
        }
        self.by_loader
            .entry(loader_ns)
            .or_default()
            .insert(Box::from(internal), answer);
        self.rows += 1;
    }

    /// Rows held (all loaders).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.rows
    }
}

/// Whether this VM's define pre-pass already had `loader_ns`'s `loadClass`
/// answer the supertype `internal` ([`SupertypeLinks`];
/// `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY`). `false` for a built-in namespace
/// (`< NATIVE_FIRST_USER_DEFINED`) and in mock contexts (no per-VM slot).
pub fn supertype_link_known(
    ctx: &dyn NativeContext,
    loader_ns: u32,
    internal: &str,
) -> bool {
    if loader_ns < cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED {
        return false;
    }
    archive_bytes_caches(ctx)
        .is_some_and(|caches| caches.supertype_links.lock().contains(loader_ns, internal))
}

/// The class `loader_ns`'s `loadClass` answered for the supertype `internal`
/// when [`supertype_link_known`] knows the link and its row kept the answer
/// (interpreter round i4 wave 96, lane L5; see [`SupertypeLinks`]). Checked
/// against the class store: an id that no longer names `internal` (an
/// unloaded class's reused slot) answers `None`.
pub fn supertype_link_answer(
    ctx: &dyn NativeContext,
    loader_ns: u32,
    internal: &str,
) -> Option<cratonvm_types::ClassId> {
    if loader_ns < cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED {
        return None;
    }
    let id = archive_bytes_caches(ctx)?
        .supertype_links
        .lock()
        .answer(loader_ns, internal)?;
    ctx.class_name_arc_of_id(id)
        .is_some_and(|name| {
            name.len() == internal.len()
                && name
                    .bytes()
                    .zip(internal.bytes())
                    .all(|(a, b)| a == b || (a == b'.' && b == b'/'))
        })
        .then_some(id)
}

/// Record that `loader_ns`'s `loadClass` answered the supertype `internal`
/// with the class `answer` ([`supertype_link_known`] answers it from now on,
/// [`supertype_link_answer`] the class). Nothing for a built-in namespace or
/// in mock contexts.
pub fn note_supertype_link(
    ctx: &dyn NativeContext,
    loader_ns: u32,
    internal: &str,
    answer: Option<cratonvm_types::ClassId>,
) {
    if loader_ns < cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED {
        return;
    }
    if let Some(caches) = archive_bytes_caches(ctx) {
        caches
            .supertype_links
            .lock()
            .insert_answer(loader_ns, internal, answer);
    }
}


/// The archive memoised under `key` in `cache`, opened by `open` on a miss
/// (outside the lock), re-stat'ed on every hit: a changed file turns the row
/// into a refusal and answers `None`, as does a refused open. The map is
/// cleared when it reaches `limit` rows. [`memoised_archive_with`] without a
/// refusal stamp.
#[cfg_attr(not(test), allow(dead_code))]
pub fn memoised_archive(
    cache: &OrderedPlMutex<ArchiveBytesCache>,
    key: &str,
    limit: usize,
    open: impl FnOnce() -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>>,
) -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>> {
    memoised_archive_with(cache, key, limit, None, open)
}

/// [`memoised_archive`] with `refusal_path`
/// (`CRATONVM_NESTED_JAR_REFUSAL_STAMP`; interpreter round i2 wave 68, lane
/// L5): when `Some`, a hit whose file changed is REBUILT (and the row
/// replaced) instead of turned into a refusal, and a refusal records the
/// stamp of the file it names and is rebuilt once that changes
/// ([`memoised_row`]). A rebuilt row carries the new file's stamp; every
/// served entry still passes the callers' gates against it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn memoised_archive_with(
    cache: &OrderedPlMutex<ArchiveBytesCache>,
    key: &str,
    limit: usize,
    refusal_path: Option<&dyn Fn() -> Option<std::path::PathBuf>>,
    open: impl FnOnce() -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>>,
) -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>> {
    memoised_archive_noted(cache, key, limit, refusal_path, &|_| {}, open)
}

/// [`memoised_archive_with`] reporting each row event to `note`
/// (interpreter round i2 wave 69, lane L5; the LOADER census).
pub fn memoised_archive_noted(
    cache: &OrderedPlMutex<ArchiveBytesCache>,
    key: &str,
    limit: usize,
    refusal_path: Option<&dyn Fn() -> Option<std::path::PathBuf>>,
    note: &dyn Fn(NestedJarRowEvent),
    open: impl FnOnce() -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>>,
) -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>> {
    memoised_archive_checked(
        cache,
        key,
        limit,
        refusal_path,
        note,
        cratonvm_classloading::NestedArchiveEntries::outer_unchanged,
        open,
    )
}

/// [`memoised_archive_noted`] with the hit's "is the file unchanged" check
/// given by the caller (interpreter round i3 wave 71, lane L5: the nested
/// bytes rows under `CRATONVM_NESTED_JAR_POOLED_STAT` leave it to the pooled
/// read, see [`nested_jar_archive`]).
pub fn memoised_archive_checked(
    cache: &OrderedPlMutex<ArchiveBytesCache>,
    key: &str,
    limit: usize,
    refusal_path: Option<&dyn Fn() -> Option<std::path::PathBuf>>,
    note: &dyn Fn(NestedJarRowEvent),
    unchanged: impl Fn(&cratonvm_classloading::NestedArchiveEntries) -> bool,
    open: impl FnOnce() -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>>,
) -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>> {
    let on_changed = if refusal_path.is_some() {
        ChangedRow::Rebuild
    } else {
        ChangedRow::Refuse
    };
    memoised_row(
        cache,
        key,
        limit,
        on_changed,
        unchanged,
        refusal_path,
        note,
        open,
    )
}

// ---------------------------------------------------------------------------
// The jar-loader bytes index (interpreter round i2 wave 64, lane L5;
// `docs/feature-designs/class-resolution-and-caching.md`).
// ---------------------------------------------------------------------------

/// The anonymous `Resource` classes of `URLClassPath$JarLoader` share this
/// prefix (`checkResource`'s `new Resource() {...}`; its number is not relied
/// on, the captured fields are).
pub const URLCLASSPATH_JAR_LOADER_ANON_PREFIX: &str = "jdk/internal/loader/URLClassPath$JarLoader$";

/// The only `JarFile` class the jar-loader bytes index serves: the one
/// `URLClassPath.JarLoader.getJarFile` builds for a `file:` URL. A subclass
/// (`sun.net.www.protocol.jar.URLJarFile` for any other URL) runs the body.
pub const JAVA_UTIL_JAR_FILE: &str = "java/util/jar/JarFile";

/// Is `name` an anonymous class of `URLClassPath$JarLoader` (`...$JarLoader$<digits>`)?
pub fn is_jar_loader_anonymous_class(name: &str) -> bool {
    name.strip_prefix(URLCLASSPATH_JAR_LOADER_ANON_PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Reviewed `cratonvm_native_api::NativeKind::Intrinsic` on `jdk/internal/loader/Resource.getBytes()[B`
/// with only the jar-loader half (`CRATONVM_JAR_LOADER_BYTES_INDEX` on,
/// `CRATONVM_NESTED_JAR_BYTES_INDEX` off). See [`resource_get_bytes`].
pub fn jar_loader_resource_get_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    resource_get_bytes(ctx, args, false, true)
}

/// Reviewed `cratonvm_native_api::NativeKind::Intrinsic` on `jdk/internal/loader/Resource.getBytes()[B`
/// with both halves (both switches on). See [`resource_get_bytes`].
pub fn nested_and_jar_loader_resource_get_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    resource_get_bytes(ctx, args, true, true)
}

/// The `Resource.getBytes` intrinsic with its halves chosen at registration:
/// `nested` -- the generic `URLClassPath$Loader`'s `Resource` over a Spring
/// Boot `jar:nested:` URL ([`nested_jar_resource_bytes`], wave 63); `jar` --
/// a `URLClassPath$JarLoader` `Resource` over a plain `JarFile`
/// ([`jar_loader_resource_bytes`], wave 64). Both serve through
/// [`serve_nested_jar_resource_bytes`] (the body's `getContentLength`, length
/// rule and `close`, with the reads skipped); every other receiver, and any
/// doubt, runs the JDK body.
///
/// For a `JarLoader` resource the JDK 25 `checkResource` class may override
/// `getBytes` (`super.getBytes()` then a CRC-32 check of the returned bytes
/// against `entry.getCrc()` that records a `dataError`); that override still
/// runs, as bytecode, around this intrinsic -- it reaches it through
/// `invokespecial Resource.getBytes` -- and checks the bytes served here,
/// which are the entry's own (the `zip` reader verified their CRC, and the
/// gate compared the `ZipEntry`'s `crc` / `size` with the VM's central
/// directory).
pub fn resource_get_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    nested: bool,
    jar: bool,
) -> MethodCallResult {
    if nested {
        match nested_jar_resource_bytes(&*ctx, args) {
            NestedJarResourceBytes::NotLoaderResource => {}
            NestedJarResourceBytes::Body => {
                ctx.note_nested_jar_bytes(false);
                return resource_get_bytes_body(ctx, args);
            }
            NestedJarResourceBytes::Serve { bytes, stream } => {
                if let Some(Value::Object(Some(this))) = args.first() {
                    ctx.note_nested_jar_bytes(true);
                    return serve_nested_jar_resource_bytes(ctx, *this, stream, bytes);
                }
                return resource_get_bytes_body(ctx, args);
            }
        }
    }
    if jar {
        match jar_loader_resource_bytes(&*ctx, args) {
            NestedJarResourceBytes::NotLoaderResource => {}
            NestedJarResourceBytes::Body => ctx.note_jar_loader_bytes(false),
            NestedJarResourceBytes::Serve { bytes, stream } => {
                if let Some(Value::Object(Some(this))) = args.first() {
                    ctx.note_jar_loader_bytes(true);
                    return serve_nested_jar_resource_bytes(ctx, *this, stream, bytes);
                }
            }
        }
    }
    resource_get_bytes_body(ctx, args)
}

/// The JDK's own `Resource.getBytes` body, bound by `invokespecial` on
/// `Resource` (the method the caller's dispatch -- virtual, or a subclass's
/// `super.getBytes()` -- selected).
///
/// gci i18/vp: a tail call (`continuation_pin::tail_call`): every caller is
/// [`resource_get_bytes`], whose answer this is, under the registered
/// `Resource.getBytes()[B` callbacks. The body reads the resource's stream
/// (a socket for a remote class-path entry), which may park.
pub fn resource_get_bytes_body(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cratonvm_native_api::continuation_pin::tail_call(|| {
        ctx.invoke_special_bytecode_only(JDK_LOADER_RESOURCE, "getBytes", "()[B", args)
    })
}

/// The gates of the jar-loader half of [`resource_get_bytes`]; every one must
/// hold to serve, any doubt runs the body:
///
/// * the receiver is an anonymous class of `URLClassPath$JarLoader`
///   ([`is_jar_loader_anonymous_class`]) with the captured `val$name`,
///   `val$entry` and `this$0` of `checkResource`'s `Resource`;
/// * the `Resource`'s `cis` is already set (the `defineClass` paths'
///   `getByteBuffer()` set it, so `cachedInputStream()` would only return it,
///   and `JarFile.getInputStream` -- with its verifier decision -- has run);
/// * the loader's `jar` is exactly a `java.util.jar.JarFile` (the `file:`
///   case), not closed (`closeRequested`), with no `JarVerifier` (`jv`);
/// * the `JarEntry`'s `name` is `val$name` (no versioned real name), its
///   `crc` and `size` are set;
/// * the VM's mapping of the `ZipFile`'s path ([`jar_loader_archive`]) has
///   the plain view (not Multi-Release, unsigned, no duplicate names), the
///   file is unchanged since it was mapped, and holds `name` as a file entry
///   whose central-directory CRC and size are the `JarEntry`'s and whose
///   bytes read cleanly (`NestedArchiveEntries::entry_bytes_matching`).
pub fn jar_loader_resource_bytes(ctx: &dyn NativeContext, args: &[Value]) -> NestedJarResourceBytes {
    let Some(Value::Object(Some(this))) = args.first() else {
        return NestedJarResourceBytes::NotLoaderResource;
    };
    let is_jar_loader_resource = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(*this))
        .as_deref()
        .is_some_and(is_jar_loader_anonymous_class);
    if !is_jar_loader_resource {
        return NestedJarResourceBytes::NotLoaderResource;
    }
    let Value::Object(Some(entry)) = ctx.get_field_by_name(*this, "val$entry") else {
        return NestedJarResourceBytes::Body;
    };
    let Value::Object(Some(name)) = ctx.get_field_by_name(*this, "val$name") else {
        return NestedJarResourceBytes::Body;
    };
    let Some(name) = ctx.read_string(name) else {
        return NestedJarResourceBytes::Body;
    };
    let Value::Object(Some(loader)) = ctx.get_field_by_name(*this, "this$0") else {
        return NestedJarResourceBytes::Body;
    };
    let Value::Object(Some(stream)) = ctx.get_field_by_name(*this, "cis") else {
        return NestedJarResourceBytes::Body;
    };
    let Value::Object(Some(jar)) = ctx.get_field_by_name(loader, "jar") else {
        return NestedJarResourceBytes::Body;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(jar))
        .as_deref()
        != Some(JAVA_UTIL_JAR_FILE)
    {
        return NestedJarResourceBytes::Body;
    }
    if !matches!(ctx.get_field_by_name(jar, "jv"), Value::Object(None)) {
        return NestedJarResourceBytes::Body;
    }
    if matches!(ctx.get_field_by_name(jar, "closeRequested"), Value::Int(v) if v != 0) {
        return NestedJarResourceBytes::Body;
    }
    let entry_name = match ctx.get_field_by_name(entry, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    if entry_name.as_deref() != Some(name.as_str()) {
        return NestedJarResourceBytes::Body;
    }
    let (Value::Long(crc), Value::Long(size)) = (
        ctx.get_field_by_name(entry, "crc"),
        ctx.get_field_by_name(entry, "size"),
    ) else {
        return NestedJarResourceBytes::Body;
    };
    let (Ok(crc), Ok(size)) = (u32::try_from(crc), u64::try_from(size)) else {
        return NestedJarResourceBytes::Body;
    };
    let Some(path) = zip_file_path(ctx, jar) else {
        return NestedJarResourceBytes::Body;
    };
    // `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT` (interpreter round i3 wave 71):
    // a jar the application class path holds unchanged answers from the class
    // path's own entry, with no row; anything else asks the rows as before.
    if let Some(answer) = jar_loader_class_path_direct(ctx, &path, &name, crc, size) {
        return match answer {
            Some(bytes) => NestedJarResourceBytes::Serve { bytes, stream },
            None => NestedJarResourceBytes::Body,
        };
    }
    let Some(archive) = jar_loader_archive(ctx, &path) else {
        return NestedJarResourceBytes::Body;
    };
    match archive.entry_bytes_matching(&name, crc, size) {
        Some(bytes) => NestedJarResourceBytes::Serve { bytes, stream },
        None => NestedJarResourceBytes::Body,
    }
}

/// The jar-loader bytes of `name` in the jar at `path` straight from the
/// VM's application class path (`CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`;
/// interpreter round i3 wave 71, lane L5;
/// `i70-L5-proposal-serve-class-path-jar-bytes-without-a-row`): the `-cp`
/// jar the JDK's `AppClassLoader` reads through `URLClassPath$JarLoader` is
/// already mapped, parsed and indexed by the class path, so the entry is read
/// through that entry itself (`ClassPath::jar_entry_bytes_matching`: one
/// `stat` against the load-time stamp, the plain view computed once per jar,
/// the same CRC / size and read rules as a row) and no row is built, kept or
/// re-checked in the jar cache.
///
/// `None` -- the caller asks the rows exactly as without the switch -- when
/// the switch is off or the class path does not hold the file unchanged;
/// `Some(None)` runs the body (a row over the same archive would refuse the
/// entry too); `Some(Some(bytes))` serves.
pub fn jar_loader_class_path_direct(
    ctx: &dyn NativeContext,
    path: &str,
    name: &str,
    crc32: u32,
    size: u64,
) -> Option<Option<Vec<u8>>> {
    if !ctx.jar_loader_class_path_direct() {
        return None;
    }
    match ctx.class_path_jar_entry_bytes(std::path::Path::new(path), name, crc32, size) {
        cratonvm_native_api::ClassPathJarBytes::Bytes(bytes) => {
            ctx.note_nested_jar_row(NestedJarRowEvent::JarLoaderClassPathDirect);
            Some(Some(bytes))
        }
        cratonvm_native_api::ClassPathJarBytes::Body => Some(None),
        cratonvm_native_api::ClassPathJarBytes::NotHeld => {
            ctx.note_nested_jar_row(NestedJarRowEvent::JarLoaderClassPathDirectFallback);
            None
        }
    }
}

/// The path a `java.util.zip.ZipFile` was opened on (`File.getPath()`, kept
/// in `filePath` in JDK 25; `name` in older layouts), `None` when neither is
/// a non-empty string.
pub fn zip_file_path(ctx: &dyn NativeContext, zip: ObjectRef) -> Option<String> {
    ["filePath", "name"]
        .into_iter()
        .find_map(|field| match ctx.get_field_by_name(zip, field) {
            Value::Object(Some(s)) => ctx.read_string(s).filter(|p| !p.is_empty()),
            _ => None,
        })
}

/// THIS VM's mapping of the jar at `path` (`ClassPath::open_jar_archive`),
/// memoised by the path like [`nested_jar_archive`]'s copies; `None` (memoised
/// too) when it is not a readable archive, and from the first ask that finds
/// the file changed on disk.
pub fn jar_loader_archive(
    ctx: &dyn NativeContext,
    path: &str,
) -> Option<Arc<cratonvm_classloading::NestedArchiveEntries>> {
    let caches = archive_bytes_caches(ctx);
    let open = || {
        // `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE` (wave 70): a jar of the
        // VM's application class path lends its own bytes and parse
        // ([`jar_loader_class_path_row`]); any doubt opens as before.
        if let Some(row) = jar_loader_class_path_row(ctx, path) {
            return Some(Arc::new(row));
        }
        let path = std::path::Path::new(path);
        let map = caches.is_some_and(|caches| caches.on_vm_class_path(ctx, path));
        cratonvm_classloading::ClassPath::open_jar_archive(path, map).map(Arc::new)
    };
    // `CRATONVM_NESTED_JAR_REFUSAL_STAMP` (wave 68): see [`memoised_archive_with`].
    let jar_path = || Some(std::path::PathBuf::from(path));
    let refusal_path = ctx
        .nested_jar_refusal_stamp()
        .then_some(&jar_path as &dyn Fn() -> Option<std::path::PathBuf>);
    let note = |event| ctx.note_nested_jar_row(event);
    match caches {
        Some(caches) => memoised_archive_noted(
            &caches.jar,
            path,
            JAR_LOADER_ARCHIVE_CACHE_LIMIT,
            refusal_path,
            &note,
            open,
        ),
        None => open(),
    }
}

/// The jar-loader row for `path` built from the VM's application class
/// path's own archive (`CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`; interpreter
/// round i2 wave 70, lane L5;
/// `i69-L5-proposal-lend-the-class-path-archive-to-the-jar-loader-bytes-index`):
/// the `-cp` jar the JDK's `AppClassLoader` reads through
/// `URLClassPath$JarLoader` is already mapped and parsed by the class path,
/// so the row shares that mapping and a clone of that parse
/// (`ClassPath::open_jar_archive_lent`) instead of mapping and parsing the
/// file a second time. `None` -- the caller opens the file as without the
/// switch -- when the switch is off, `path` is not an archive of the
/// application class path (exact, then canonical, comparison), or the file
/// no longer has the stamp (length, modification time) it had when the class
/// path loaded it (`ClassPath::outer_archive_for`). Consulted on a row BUILD
/// only: the row carries the load-time stamp, so the memo's re-stat on every
/// hit drops it once the file is replaced, and every served entry is still
/// checked against the `ZipEntry`'s CRC and size.
pub fn jar_loader_class_path_row(
    ctx: &dyn NativeContext,
    path: &str,
) -> Option<cratonvm_classloading::NestedArchiveEntries> {
    if !ctx.jar_loader_class_path_archive() {
        return None;
    }
    let lent = ctx.class_path_outer_archive(std::path::Path::new(path))?;
    let lent = lent
        .downcast::<cratonvm_classloading::OuterArchive>()
        .ok()?;
    let row = cratonvm_classloading::ClassPath::open_jar_archive_lent(*lent)?;
    ctx.note_nested_jar_row(NestedJarRowEvent::JarLoaderClassPathLent);
    Some(row)
}

/// Initialize a URLClassLoader: store the URL array, extract paths, register with classpath.
pub fn ucl_setup(ctx: &mut dyn NativeContext, this: ObjectRef, urls: Value, parent: Value) {
    ctx.set_field(this, UCL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    ctx.set_field(this, UCL_PARENT_REF, parent);
    ctx.set_field(this, UCL_CLOSED, Value::Int(0));
    ucl_mark_open(ctx, this);

    let url_arr = match urls {
        Value::Object(Some(arr)) => arr,
        _ => {
            ctx.set_field(this, UCL_URL_COUNT, Value::Int(0));
            return;
        }
    };

    let count = ctx.array_length(url_arr);
    // GC-safety: `new_array` below can trigger a moving GC; `this` and
    // `url_arr` (the caller-supplied source array, read from in the copy
    // loop) are both reused afterward, unpinned otherwise.
    let this_pin = ctx.pin_native_root(this);
    let url_arr_pin = ctx.pin_native_root(url_arr);
    // Copy URLs into a storage array and extract paths for classpath registration.
    let storage = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count.max(16));
    let this = ctx.read_native_pin(this_pin, this);
    let url_arr = ctx.read_native_pin(url_arr_pin, url_arr);
    ctx.unpin_native_roots(this_pin);
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        let elem = ctx.get_array_element(url_arr, i);
        ctx.set_array_element(storage, i, elem);
        if let Value::Object(Some(url_obj)) = elem {
            if let Some(p) = extract_url_path(ctx, url_obj) {
                paths.push(p);
            }
        }
    }
    ctx.set_field(this, UCL_URLS_ARRAY, Value::Object(Some(storage)));
    ctx.set_field(this, UCL_URL_COUNT, Value::Int(count as i32));

    if !paths.is_empty() {
        ctx.register_dynamic_classpath(&paths);
        tracing::debug!(
            "URLClassLoader.<init>: registered {} URLs to classpath",
            paths.len()
        );
    }
}

/// Legacy compatibility slot for URLClassPath instances created by older
/// synthetic paths. Real-JDK URLClassLoader constructor URLs are retained in
/// the named `path` ArrayList instead, because raw slot zero aliases that field.
pub const UCP_STASHED_URLS: usize = 0;

/// True when `ucp` carries CratonVM's SYNTHETIC `URLClassPath` layout — i.e.
/// [`UCP_STASHED_URLS`] (slot 0) really is our stash array.
///
/// Real `jdk.internal.loader.URLClassPath` declares `path`(0) — an
/// `ArrayList<URL>`, NOT an array. `array_length` of a non-array answers 0
/// (`vm_exec.rs` guards the kind), so the unguarded slot-0 read did not crash;
/// it silently reported "this loader owns no URLs", which is a wrong answer of
/// exactly the kind a URL-visibility fix is trying to avoid. Reachable
/// whenever `record_url_on_path` has created the list and `ucp_path_urls` has
/// still declined it — an empty `path`.
///
/// Class-side for the same reason [`cl_has_synthetic_layout`] is: a value-shape
/// test cannot do this job, because `get_field_by_name` answers
/// `Value::Object(None)` both for a name it cannot resolve and for a real
/// reference field that is null (`vm/src/vm/vm_exec.rs`, and the trait contract
/// in `native-api/src/registry.rs`). `URLClassPath` is not a `ClassLoader`, so
/// it needs its own witness rather than `cl_has_synthetic_layout`'s three.
pub fn ucp_synthetic_layout(ctx: &dyn NativeContext, ucp: ObjectRef) -> bool {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(ucp), "path")
        .is_none()
}

/// Is the value stashed at [`UCP_STASHED_URLS`] genuinely a reference ARRAY?
///
/// This is the question `ucl_get_urls`'s stash fallback actually needs, and it
/// is NOT the same as "does this `ucp` have a synthetic layout". Gating the
/// stash on [`ucp_synthetic_layout`] looked right but broke a load-bearing
/// real-JDK path: [`record_ucl_urls`] deliberately stashes a real-mode
/// `URLClassLoader`'s constructor `URL[]` on its `ucp` PLACEHOLDER precisely
/// because the loader itself has the real field layout — see that function's
/// doc comment, and the Tomcat `StandardJarScanner` TLD regression it names.
/// A real-layout `ucp` with a real stash is therefore an expected shape, not a
/// contradiction, and the synthetic-layout gate refused it.
///
/// Asking about the stashed value answers the actual safety concern directly:
/// on a REAL `jdk.internal.loader.URLClassPath`, slot 0 is `path`, an
/// `ArrayList`, and `ObjectHeader::array_length` answers **0** for any
/// non-array — so a real `path` contributes nothing and is never indexed.
///
/// A length test rather than a class-name test, deliberately. Array objects do
/// not carry a nameable class in every context (`MockNativeContext` assigns
/// them `ClassId::new(0)`, whose name is `None`), so a `starts_with('[')`
/// witness silently answers "not an array" for genuine arrays. The length is
/// the one property both the real heap and the mock agree on.
///
/// An EMPTY stash answers `false` here, and that is correct rather than merely
/// tolerable: reading a zero-length array would contribute no URLs anyway, so
/// both arms produce the same result.
pub fn ucp_stash_is_reference_array(ctx: &dyn NativeContext, stashed: ObjectRef) -> bool {
    ctx.array_length(stashed) > 0
}

/// Record a real-JDK-mode `URLClassLoader`'s constructor `URL[]` so that
/// `getURLs()` returns the URLs the loader was built with.
///
/// The real-JDK-mode `URLClassLoader.<init>` natives (see `classloader_real`)
/// wire a loader's URLs into CratonVM's GLOBAL dynamic classpath (so classes
/// load) but never store them per-instance. Real `getURLs()` reads
/// `ucp.getURLs()`, and our `URLClassPath` shim returns empty — so `getURLs()`
/// yielded `[]`. That broke any code that walks a classloader's URLs, e.g.
/// Tomcat's `StandardJarScanner`, which scans the classloader hierarchy via
/// `getURLs()` to find TLDs: a TLD in a JAR added to a parent `URLClassLoader`
/// (outside `/WEB-INF/lib`) was invisible, 500-ing JSPs that referenced it
/// (`TestTagLibraryInfoImpl.testExternalTaglibDependantUsesUri`).
///
/// The synthetic per-instance slots used by `ucl_setup`/`ucl_get_urls` can't be
/// reused here: a real `java.net.URLClassLoader` has the real JDK field layout,
/// so those raw indices land on unrelated reference-typed fields (writing an
/// `Int` count there does not read back as a count). Instead stash the original
/// `URL[]` on the loader's `ucp` placeholder, which `ucl_get_urls` reads back.
pub fn record_ucl_urls(ctx: &mut dyn NativeContext, this: ObjectRef, urls: Value) {
    let url_arr = match urls {
        Value::Object(Some(arr)) => arr,
        _ => return,
    };
    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
        // The real-mode URLClassLoader constructors call this helper directly.
        // Retain URLs in `ucp.path`, which backs both getURLs and receiver-local
        // class/resource lookup. Do not use raw slot zero: on real JDKs it is
        // the `path` field itself, so writing the URL[] there corrupts the list.
        //
        // `record_url_on_path` can allocate and move both the array and the
        // placeholder, so keep both rooted and reload them on every iteration.
        let p_urls = ctx.pin_native_root(url_arr);
        let p_ucp = ctx.pin_native_root(ucp);
        let count = ctx.array_length(url_arr);
        for i in 0..count {
            let urls = ctx.read_native_pin(p_urls, url_arr);
            if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                let ucp = ctx.read_native_pin(p_ucp, ucp);
                record_url_on_path(ctx, ucp, url);
            }
        }
        ctx.unpin_native_roots(p_urls);
    }
}

pub fn ucl_init_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let urls = args.get(1).copied().unwrap_or(Value::Object(None));
    crate::classloader_real::init_urlclassloader_constructor_with_default_parent(ctx, this, urls)?;
    Ok(None)
}

pub fn ucl_init_urls_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let urls = args.get(1).copied().unwrap_or(Value::Object(None));
    let parent = args.get(2).copied().unwrap_or(Value::Object(None));
    crate::classloader_real::init_urlclassloader_constructor_with_parent(ctx, this, urls, parent)?;
    Ok(None)
}

pub fn ucl_init_urls_parent_factory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // factory arg ignored
    ucl_init_urls_parent(ctx, args)
}

pub fn ucl_find_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `URLClassLoader.findClass(String)` — reuse the base parent-first
    // delegation as a permissive findClass. Route to the base delegation
    // DIRECTLY (not `cl_load_class`) so it never re-triggers the
    // `loadClass(String,boolean)` override-dispatch: a subclass that overrides
    // loadClass and calls `super.findClass`/`findClass` from inside that
    // override would otherwise recurse back into its own loadClass.
    let this = obj_arg(args, 0)?;
    if ucl_is_closed(ctx, this) {
        let name = args
            .get(1)
            .and_then(|value| match value {
                Value::Object(Some(name)) => ctx.read_string(*name),
                _ => None,
            })
            .unwrap_or_else(|| "<unknown>".to_owned());
        let exception = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exception?,
        ));
    }
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = ctx.read_string(name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");
    // gc-common w20-f: the local URL search may wait for a peer definer, fetch
    // or define (GC-capable) even on a miss, and the delegation below used the
    // loader and the name at their entry addresses. Pinned across it and
    // re-read, as `cl_load_class_base_delegation_rooted` does around its own
    // call.
    let this_pin = ctx.pin_native_root(this);
    let name_pin = ctx.pin_native_root(name_obj);
    let local = ucl_try_define_local_class(ctx, this, &internal);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    let name_obj: ObjectRef = ctx.read_native_pin(name_pin, name_obj);
    ctx.unpin_native_roots(this_pin);
    if let Some(result) = local {
        return result;
    }
    cl_load_class_base_delegation(ctx, this, name_obj)
}

// ---------------------------------------------------------------------------
// Real-mode `URLClassLoader.addURL` + custom-handler resource resolution.
//
// In real-JDK mode CratonVM serves class/resource loading from its global
// dynamic classpath and shims `jdk.internal.loader.URLClassPath` down to
// safe stubs (see `register_url_class_path_safe_stubs`); the `ucp` field is a
// bare synthetic `URLClassPath` with all instance fields null. The real
// `URLClassLoader.addURL` bytecode therefore NPEs at
// `URLClassPath.addURL` → `synchronized (unopenedUrls)` (null monitor). The
// native below replaces it: it records each added URL on `ucp.path` (a real
// `ArrayList<URL>` we create on demand) and, for ordinary `file:`/`jar:` URLs,
// also extends the global dynamic classpath.
//
// Recording the URLs lets `ucl_find_resource(s)` resolve resources that live
// behind an application-supplied `URLStreamHandler` — most notably ShrinkWrap's
// in-memory `archive:` handler, whose `JavaArchive` is reachable ONLY through
// the handler (there is no filesystem path the global walk could find). For
// such a base URL we build `new URL(base, name)` (which inherits the handler)
// and probe it through the handler; a hit is returned as a real `java.net.URL`
// whose `openStream()`/`openConnection()` reach the archive (see
// `net_phase_e::url_custom_handler_connection`). Hibernate's
// `NoDepthTests` JPA variants discover `META-INF/persistence.xml` this way.
// ---------------------------------------------------------------------------

/// True iff `url` carries a non-null, application-provided `URLStreamHandler`
/// (i.e. not a JDK `sun.net.*` built-in, and not a CratonVM synthetic URL whose
/// `handler` field is null). Pure field reads — never allocates.
pub fn url_has_custom_handler(ctx: &dyn NativeContext, url: ObjectRef) -> bool {
    let handler = match ctx.get_field_by_name(url, "handler") {
        Value::Object(Some(h)) => h,
        _ => return false,
    };
    let hclass = ctx
        .class_name_of_id(ctx.class_id_of_object(handler))
        .unwrap_or_default();
    !hclass.starts_with("sun/net/")
}

/// Construct `new URL(base, name)` via the real JDK `URL(URL,String)`
/// constructor, so the result inherits `base`'s handler and resolves `name`
/// relative to it. Returns the fresh `URL` (the caller must pin it before any
/// further allocation) or `None` if construction throws.
pub fn resolve_url_against(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    name: &str,
) -> Option<ObjectRef> {
    // `create_string` can relocate `base`.
    let p_base = ctx.pin_native_root(base);
    let name_str = ctx.create_string(name);
    let base = ctx.read_native_pin(p_base, base);
    // `new_object` can relocate `base` / `name_str`.
    let p_name = ctx.pin_native_root(name_str);
    let url = match ctx.new_object("java/net/URL") {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(p_base);
            return None;
        }
    };
    // `URL.<init>` allocates internally — pin the receiver and both args.
    let p_url = ctx.pin_native_root(url);
    let base = ctx.read_native_pin(p_base, base);
    let name_str = ctx.read_native_pin(p_name, name_str);
    let r = ctx.invoke_special(
        "java/net/URL",
        "<init>",
        "(Ljava/net/URL;Ljava/lang/String;)V",
        &[
            Value::Object(Some(url)),
            Value::Object(Some(base)),
            Value::Object(Some(name_str)),
        ],
    );
    let url = ctx.read_native_pin(p_url, url);
    let result = match r {
        Ok(_) => {
            let base = ctx.read_native_pin(p_base, base);
            if let Value::Object(Some(handler)) = ctx.get_field_by_name(base, "handler") {
                // `URL(URL, String)` normally copies this private field.  Keep
                // that invariant explicit for native construction paths too;
                // a process-global side table keyed by identity hash can collide
                // after collection and apply an old archive handler to a later
                // unrelated URL.
                ctx.set_field_by_name(url, "handler", Value::Object(Some(handler)));
            }
            Some(url)
        }
        Err(_) => None,
    };
    ctx.unpin_native_roots(p_base);
    result
}

/// Probe whether `url` resolves to an existing resource through its custom
/// handler: `handler.openConnection(url).getInputStream()` must yield a
/// non-null stream without throwing. The probe stream is closed immediately.
pub fn probe_resource_exists(ctx: &mut dyn NativeContext, url: ObjectRef) -> bool {
    let conn = match crate::net_phase_e::url_custom_handler_connection(ctx, url) {
        Ok(Some(Value::Object(Some(c)))) => c,
        _ => return false, // null connection, no custom handler, or threw
    };
    let p_conn = ctx.pin_native_root(conn);
    let conn = ctx.read_native_pin(p_conn, conn);
    let r = ctx.invoke_virtual(conn, "getInputStream", "()Ljava/io/InputStream;", &[]);
    ctx.unpin_native_roots(p_conn);
    match r {
        Ok(Some(Value::Object(Some(stream)))) => {
            // Close the probe stream so we don't leak it (ShrinkWrap tracks
            // opened streams for cleanup on classloader close).
            //
            // KEPT SWALLOW, at JDK parity. `URLClassPath$Loader.getResource`
            // wraps its whole `url.openConnection()` / `getInputStream()`
            // region in `catch (Exception e) { return null; }`, so a failure
            // anywhere in the probe is "no such resource", not a thrown
            // exception out of `getResources`. This predicate returns `bool`
            // and its caller holds two native pins across the call, so it
            // cannot propagate without a signature change; the residual is
            // that an `Error` is absorbed here where the JDK's `catch
            // (Exception)` would let it out. Recorded, not silently kept.
            // W7-57-close-flush-swallow-sweep.md
            let _ = ctx.invoke_virtual(stream, "close", "()V", &[]);
            true
        }
        _ => false, // null stream (directory node) or FileNotFoundException
    }
}

pub fn object_extends(ctx: &dyn NativeContext, obj: ObjectRef, target: &str) -> bool {
    let mut class_id = ctx.class_id_of_object(obj);
    for _ in 0..64 {
        match ctx.class_name_arc_of_id(class_id).as_deref() {
            Some(name) if name == target => return true,
            None => return false,
            _ => {}
        }
        match ctx.superclass_of(class_id) {
            Some(parent) if parent != class_id => class_id = parent,
            _ => return false,
        }
    }
    false
}

/// A URLClassLoader whose parent is bootstrap/platform cannot delegate an
/// application class to the process-wide application loader. Its recorded URL
/// list is its complete application view (Spring's ModifiedClassPathClassLoader
/// uses this shape to remove selected JARs from a test's classpath).
pub fn is_platform_class_loader(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(loader))
        .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
        || platform_loader_of(loader_tables(&*ctx))
            .is_some_and(|platform| platform.as_ptr() == loader.as_ptr())
}

/// Mirror of [`is_platform_class_loader`] for the process-wide application
/// (system) loader singleton: `loader` is either literally
/// `jdk.internal.loader.ClassLoaders$AppClassLoader` by declared class, or
/// pointer-identical to the recorded singleton. Unlike
/// [`is_user_defined_loader`], which answers `false` for a bare
/// `java.net.URLClassLoader`/`ClassLoader` instance because its OWN declared
/// class is on `is_builtin_loader_class`'s name list (a check aimed at a
/// different question -- see that predicate's doc comment), this asks
/// specifically "is this object THE application loader", so a bare
/// `URLClassLoader` whose parent is some OTHER user loader (not the app
/// singleton, not platform) correctly answers `false` here too and is left
/// to the caller's own "not built-in" handling.
pub fn is_application_class_loader(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(loader))
        .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$AppClassLoader")
        || app_loader_of(loader_tables(&*ctx)).is_some_and(|app| app.as_ptr() == loader.as_ptr())
}

pub fn url_classloader_isolated_from_app(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    // Some real-JDK subclasses lose their inherited URLClassLoader identity
    // at native dispatch sites. Their constructor URL list is retained by our
    // URL loader shims, so accept that authoritative signal as well.
    if !object_extends(ctx, loader, "java/net/URLClassLoader")
        && loader_constructor_url_paths(ctx, loader).is_empty()
    {
        return false;
    }
    match ctx.get_field_by_name(loader, "parent") {
        Value::Object(None) | Value::Int(0) | Value::Long(0) => true,
        Value::Object(Some(parent)) => is_platform_class_loader(ctx, parent),
        _ => false,
    }
}

pub fn is_url_class_path_object(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    object_extends(ctx, obj, "jdk/internal/loader/URLClassPath")
        || object_extends(ctx, obj, "sun/misc/URLClassPath")
}

pub fn is_array_list_object(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    object_extends(ctx, obj, "java/util/ArrayList")
}

/// Build a `java.util.ArrayList<URL>` of resources named `name` reachable
/// through the loader's custom-handler base URLs (recorded on `ucp.path` by
/// `ucl_add_url_real`). Returns `None` when the loader has no such base URLs or
/// none resolve — the common case for ordinary `file:`/`jar:` loaders, leaving
/// their behaviour untouched.
pub fn build_custom_handler_url_list(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    name: &str,
) -> Option<ObjectRef> {
    let ucp = match ctx.get_field_by_name(loader, "ucp") {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    if !is_url_class_path_object(ctx, ucp) {
        return None;
    }
    // URLClassPath's real `path` field is not a stable writable extension
    // point across JDK builds.  Constructors always retain their URLs in our
    // dedicated placeholder slot, so use that array when no usable path list
    // is exposed.
    let (path_list, array_backed) = match ctx.get_field_by_name(ucp, "path") {
        Value::Object(Some(o)) if is_array_list_object(ctx, o) => (o, false),
        _ => match ctx.get_field(ucp, UCP_STASHED_URLS) {
            Value::Object(Some(o)) => (o, true),
            _ => return None,
        },
    };
    // Fast path: skip the work entirely unless at least one recorded base URL
    // actually carries a custom handler (ordinary loaders record only
    // file:/jar: URLs, whose handler is null/`sun.net.*`).
    let p_path = ctx.pin_native_root(path_list);
    let size = if array_backed {
        ctx.array_length(path_list) as i32
    } else {
        match ctx.invoke_virtual(path_list, "size", "()I", &[]) {
            Ok(Some(Value::Int(n))) => n,
            _ => {
                ctx.unpin_native_roots(p_path);
                return None;
            }
        }
    };
    let result = match ctx.new_object("java/util/ArrayList") {
        Ok(Some(Value::Object(Some(l)))) => l,
        _ => {
            ctx.unpin_native_roots(p_path);
            return None;
        }
    };
    let p_result = ctx.pin_native_root(result);
    let _ = ctx.invoke_special(
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(ctx.read_native_pin(p_result, result)))],
    );
    let mut matched = 0u32;
    for i in 0..size {
        let path_list = ctx.read_native_pin(p_path, path_list);
        let base = if array_backed {
            match ctx.get_array_element(path_list, i as usize) {
                Value::Object(Some(b)) => b,
                _ => continue,
            }
        } else {
            match ctx.invoke_virtual(path_list, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)]) {
                Ok(Some(Value::Object(Some(b)))) => b,
                _ => continue,
            }
        };
        let p_base = ctx.pin_native_root(base);
        if url_has_custom_handler(ctx, base) {
            if let Some(resolved) = resolve_url_against(ctx, base, name) {
                let p_res = ctx.pin_native_root(resolved);
                let resolved = ctx.read_native_pin(p_res, resolved);
                if probe_resource_exists(ctx, resolved) {
                    let resolved = ctx.read_native_pin(p_res, resolved);
                    let result = ctx.read_native_pin(p_result, result);
                    let _ = ctx.invoke_virtual(
                        result,
                        "add",
                        "(Ljava/lang/Object;)Z",
                        &[Value::Object(Some(resolved))],
                    );
                    matched += 1;
                }
            }
        }
        // Release this iteration's pins (p_base and any p_res after it).
        ctx.unpin_native_roots(p_base);
    }
    let result = ctx.read_native_pin(p_result, result);
    ctx.unpin_native_roots(p_path); // releases p_path, p_result and the rest
    if matched == 0 {
        None
    } else {
        Some(result)
    }
}

/// Record `url` on `ucp.path` (a real `ArrayList<URL>`, created on demand). All
/// re-entrant Java calls are pinned so a moving GC can't stale the refs
/// mid-sequence.
pub fn record_url_on_path(ctx: &mut dyn NativeContext, ucp: ObjectRef, url: ObjectRef) {
    let p_ucp = ctx.pin_native_root(ucp);
    let p_url = ctx.pin_native_root(url);
    let list = match ctx.get_field_by_name(ucp, "path") {
        Value::Object(Some(l)) if is_array_list_object(ctx, l) => l,
        _ => {
            // Lazily create the `path` ArrayList and store it on `ucp`.
            let created = match ctx.new_object("java/util/ArrayList") {
                Ok(Some(Value::Object(Some(l)))) => l,
                _ => {
                    ctx.unpin_native_roots(p_ucp);
                    return;
                }
            };
            let p_list = ctx.pin_native_root(created);
            let created = ctx.read_native_pin(p_list, created);
            let _ = ctx.invoke_special(
                "java/util/ArrayList",
                "<init>",
                "()V",
                &[Value::Object(Some(created))],
            );
            let ucp = ctx.read_native_pin(p_ucp, ucp);
            let created = ctx.read_native_pin(p_list, created);
            ctx.set_field_by_name(ucp, "path", Value::Object(Some(created)));
            created
        }
    };
    let p_list = ctx.pin_native_root(list);
    let list = ctx.read_native_pin(p_list, list);
    let url = ctx.read_native_pin(p_url, url);
    let _ = ctx.invoke_virtual(
        list,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(url))],
    );
    ctx.unpin_native_roots(p_ucp); // releases every pin taken here
}

pub fn empty_enumeration_impl(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    Ok(enm)
}

/// Cached `cratonvm_classloading::ClassPath::new(paths)` construction, keyed
/// by the exact `paths` vector.
///
/// PERF (2026-07-23): both call sites below (`loader_local_resource_urls`
/// and `ucl_try_define_local_class`) used to call `ClassPath::new(&paths)`
/// FRESH on every single invocation — i.e. every `URLClassLoader.findClass`/
/// `findResource` call re-read and re-parsed every jar on the loader's
/// classpath from scratch, with no caching at all (unlike
/// `jar_contents_cached`, which this superficially resembles but doesn't
/// share any code with). `ClassPath::new` opens and parses every classpath
/// entry eagerly, so on a large classpath (`module/spring-boot-data-redis`'s
/// test classpath has ~121 jars, several of them large — testcontainers.jar
/// alone is 12.5k entries) this made ordinary Spring context bootstrap,
/// which does hundreds of `ClassUtils.isPresent()`-style lookups per
/// `ApplicationContextRunner.run()`, pay a full classpath re-scan on EVERY
/// lookup — measured ~10-12s for just 100 lookups, when the underlying work
/// should be milliseconds after the first scan. `DataRedisAutoConfigurationTests`,
/// `DataRedisAutoConfigurationJedisTests`,
/// `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests`, and
/// `DataRedisHealthContributorAutoConfigurationTests` all HANG (300s suite
/// timeout) as a direct result. Cache by the exact paths vector: a
/// `URLClassLoader.addURL` call naturally produces a longer paths vector, so
/// it transparently gets its own fresh (correct) cache entry rather than
/// serving a stale one — no explicit invalidation needed.
///
/// BOUNDED (gc-common w9-a): at most [`LOCAL_URL_CLASS_PATH_CACHE_LIMIT`]
/// entries, evicting the least recently used. "Small in practice" did not
/// hold. Every URL loader of every VM in the process adds entries, per-test
/// loaders over fresh temp directories never repeat a key, and a loader that
/// `addURL`s k jars one at a time with a lookup in between leaves k entries
/// behind, parsing 1, 2, ..., k jars each. Nothing ever removed one. LRU
/// rather than the clear-all its unread twin used keeps the hot classpaths
/// of the loaders actually in use (the DataRedis case above is one classpath
/// looked up hundreds of times).
///
/// PER VM (interpreter round i2 wave 65, lane L5): the cache lives in THIS
/// VM's [`ArchiveBytesCaches`] (`url_class_path`), not in a process global
/// (AGENTS.md: no process globals), so one VM's `ClassPath` -- the jars as
/// they were when IT first asked -- is never another VM's answer. A context
/// with no per-VM slot (mock contexts) builds the `ClassPath` for this ask
/// only.
///
/// `loader` (interpreter round i4 wave 97, lane L5): the namespace id of the
/// loader asking. The first ask of each loader other than the one the row
/// last answered re-checks the row's archives on disk
/// (`ClassPath::url_loader_archives_unchanged`, one `stat` per archive) and
/// rebuilds it when one changed, so a NEW `URLClassLoader` over the same URLs
/// sees a jar the program rewrote in between, as HotSpot's fresh
/// `URLClassPath` does (`L5W64JarLoaderBytesIndex` round 2,
/// `L5W65NestedJarWalkRewrite`); repeated asks of one loader stay a map hit.
pub fn cached_class_path_for_paths(
    ctx: &dyn NativeContext,
    paths: &[String],
    loader: u32,
) -> std::sync::Arc<cratonvm_classloading::ClassPath> {
    let cp = match archive_bytes_caches(ctx) {
        Some(caches) => class_path_from_cache_for(&caches.url_class_path, paths, Some(loader)),
        None => Arc::new(cratonvm_classloading::ClassPath::for_url_loader(paths)),
    };
    // Multi-release jars at the booted library's feature version (one
    // relaxed store; the row is this VM's own). Interpreter round i5 wave
    // 100, lane L5 (`i100-L5-multi-release-jars-read-at-version-25-...`).
    // Wave 103: capped by `jdk.util.jar.version` and turned off by
    // `jdk.util.jar.enableMultiRelease=false` (`JarFile.<clinit>`), derived
    // once per class path (the property reads may reach Java's `System.props`).
    if cp.multi_release_feature_is_set() {
        return cp;
    }
    let feature = ctx.jdk_feature_version();
    let jar_version = ctx.get_system_property("jdk.util.jar.version");
    let enable_multi_release = ctx.get_system_property("jdk.util.jar.enableMultiRelease");
    // Unknown feature, no property: 25, the class path's own default.
    cp.set_multi_release_feature(cratonvm_classloading::ClassPath::jar_runtime_version(
        feature.unwrap_or(25),
        jar_version.as_deref(),
        enable_multi_release.as_deref(),
    ));
    cp
}

/// [`cached_class_path_for_paths`]'s LRU over one cache, with no loader (a
/// hit is never re-checked).
#[cfg_attr(not(test), allow(dead_code))]
pub fn class_path_from_cache(
    cache: &OrderedPlMutex<UrlClassPathCache>,
    paths: &[String],
) -> std::sync::Arc<cratonvm_classloading::ClassPath> {
    class_path_from_cache_for(cache, paths, None)
}

/// [`cached_class_path_for_paths`]'s LRU over one cache; `loader`: see there.
pub fn class_path_from_cache_for(
    cache: &OrderedPlMutex<UrlClassPathCache>,
    paths: &[String],
    loader: Option<u32>,
) -> std::sync::Arc<cratonvm_classloading::ClassPath> {
    let hit = {
        let mut guard = cache.lock();
        let (map, tick) = &mut *guard;
        *tick += 1;
        let now = *tick;
        match map.get_mut(paths) {
            Some((cp, last_used, answered)) => {
                *last_used = now;
                let recheck = loader.is_some_and(|loader| *answered != Some(loader));
                if !recheck {
                    return Arc::clone(cp);
                }
                Some(Arc::clone(cp))
            }
            None => None,
        }
    };
    // The re-check `stat`s outside the cache lock.
    if let Some(cp) = hit {
        if cp.url_loader_archives_unchanged() {
            let mut guard = cache.lock();
            if let Some((cached, _, answered)) = guard.0.get_mut(paths) {
                if Arc::ptr_eq(cached, &cp) {
                    *answered = loader;
                }
            }
            return cp;
        }
    }
    // Construct outside the cache lock: opening a manifest pathing JAR can
    // touch hundreds of dependency archives and must not serialize unrelated
    // URLClassLoader resolution work.
    //
    // `for_url_loader` (interpreter round i4 wave 97, lane L5): this cache
    // outlives the loader (and its `close()`), so on Windows its jars are
    // read, never mapped -- a mapping held here made Windows refuse the
    // program's own rewrite, rename over or delete of a jar it had put on a
    // `URLClassLoader` (`ERROR_USER_MAPPED_FILE`; `AccessDeniedException` on
    // a rename over it), where HotSpot holds nothing once the loader is
    // closed. Reached under `--compatible` with the JDK's own
    // `URLClassLoader` bodies too: `ClassLoader.getResource` /
    // `getResources` (`cl_get_resource` -> `ucl_find_resource`) and
    // `getDefinedPackage`'s local-URL probe.
    let cp = Arc::new(cratonvm_classloading::ClassPath::for_url_loader(paths));
    let mut guard = cache.lock();
    let (map, tick) = &mut *guard;
    *tick += 1;
    let now = *tick;
    if let Some((existing, last_used, answered)) = map.get_mut(paths) {
        // Another thread built one meanwhile: keep the newer build (this
        // one, read after any change the re-check saw).
        *existing = Arc::clone(&cp);
        *last_used = now;
        *answered = loader;
        return cp;
    }
    if map.len() >= LOCAL_URL_CLASS_PATH_CACHE_LIMIT {
        let oldest = map
            .iter()
            .min_by_key(|(_, (_, used, _))| *used)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            map.remove(&oldest);
        }
    }
    map.insert(paths.to_vec(), (Arc::clone(&cp), now, loader));
    cp
}

/// Paths vector → (`ClassPath`, last-use tick, the namespace id of the loader
/// the row last answered), plus the tick counter. See
/// [`cached_class_path_for_paths`].
pub type UrlClassPathCache = (
    std::collections::HashMap<
        Vec<String>,
        (Arc<cratonvm_classloading::ClassPath>, u64, Option<u32>),
    >,
    u64,
);

/// An empty [`UrlClassPathCache`]. `Scratch` (L0): every critical section is
/// a map probe, an insert or a 64-entry eviction scan. `ClassPath::new` runs
/// outside it.
pub fn new_url_class_path_cache() -> OrderedPlMutex<UrlClassPathCache> {
    OrderedPlMutex::new((std::collections::HashMap::new(), 0), LockLevel::Scratch)
}

/// Whether `loader`'s recorded constructor URLs give it at least one local
/// path, i.e. [`ucl_try_define_local_class`] searched its own URLs and its
/// `None` is a miss over them rather than "no URLs known" (the
/// `--compatible` `URLClassLoader.findClass` stand-in,
/// `classloader_real::ucl_real_find_class`; interpreter round i7 wave 136,
/// lane L5).
///
/// Interpreter round i7 wave 137, lane L5 (review of wave 136): only when
/// EVERY constructor URL is one that search serves -- `file:`, `jar:`,
/// `nested:`, `http:` / `https:` ([`ucl_url_protocol_searched`]). A URL of
/// another protocol (a custom `URLStreamHandler`: `vfs:`, `bundle:`,
/// `memory:`, `jrt:`) is still read into a "path" by [`extract_url_path`]
/// (its `path` field), but the local search cannot open it, so a miss over
/// the readable URLs is not the loader's answer: such a loader keeps the
/// global route, as before wave 136.
pub fn ucl_has_local_url_paths(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    let urls = loader_constructor_url_objects(ctx, loader);
    !urls.is_empty()
        && urls.iter().all(|&url| ucl_url_protocol_searched(ctx, url))
        && !loader_constructor_url_paths(ctx, loader).is_empty()
}

/// Is `url`'s protocol one the receiver-local URL search
/// ([`ucl_try_define_local_class`]) serves? Read by name (a real-JDK `URL`),
/// else from the synthetic layout's slot 0, as [`http_base_from_url`] reads
/// it. An unreadable protocol answers `false`.
pub fn ucl_url_protocol_searched(ctx: &dyn NativeContext, url: ObjectRef) -> bool {
    let protocol = match ctx.get_field_by_name(url, "protocol") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => match ctx.get_field(url, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    };
    matches!(
        protocol.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("file" | "jar" | "nested" | "http" | "https")
    )
}

pub fn loader_constructor_url_paths(ctx: &dyn NativeContext, loader: ObjectRef) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for url in loader_constructor_url_objects(ctx, loader) {
        if let Some(path) = extract_url_path(ctx, url) {
            if !path.is_empty() && !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// The `java.net.URL` objects [`loader_constructor_url_paths`] reads its
/// paths from, in its order (duplicates kept). Reads only; no allocation.
pub fn loader_constructor_url_objects(ctx: &dyn NativeContext, loader: ObjectRef) -> Vec<ObjectRef> {
    // SBR2-LOADERS: the JDK-built app loader carries the class path in its real
    // `ucp`; that is NOT a private URL search path (it would make the app
    // loader look like an isolated `URLClassLoader` and cut it off from the
    // module-path classes the VM serves from the flat store).
    if object_extends(ctx, loader, "jdk/internal/loader/BuiltinClassLoader") {
        return Vec::new();
    }
    let mut out = Vec::new();

    let mut append_url = |url: ObjectRef| out.push(url);

    // Synthetic-JDK URLClassLoader instances store constructor URLs directly on
    // the loader. Real-JDK instances stash the original URL[] on the shimmed ucp
    // placeholder (see `record_ucl_urls`). The slot reads are gated because on
    // the real layout slot 4 is `parallelLockMap` and slot 2 is
    // `unnamedModule` — see `ucl_get_urls` for the full layout.
    if cl_has_synthetic_layout(ctx, loader) {
        if let Value::Object(Some(urls)) = ctx.get_field(loader, UCL_URLS_ARRAY) {
            let count = match ctx.get_field(loader, UCL_URL_COUNT) {
                Value::Int(n) if n > 0 => (n as usize).min(ctx.array_length(urls)),
                _ => 0,
            };
            for i in 0..count {
                if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                    append_url(url);
                }
            }
        }
    }

    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(loader, "ucp") {
        // `record_ucl_urls` mirrors constructor URLs on `URLClassPath.path`
        // as well as in its private compatibility slot.  The real JDK's
        // URLClassPath layout can overwrite that numeric slot while executing
        // constructor bytecode, whereas the named `path` list survives.  Read
        // it first so local URLClassLoader lookups remain isolated even after
        // several temporary test loaders have extended the global classpath.
        if let Value::Object(Some(path_list)) = ctx.get_field_by_name(ucp, "path") {
            let element_data = ctx.get_field_by_name(path_list, "elementData");
            let size = match ctx.get_field_by_name(path_list, "size") {
                Value::Int(n) if n > 0 => n as usize,
                _ => 0,
            };
            if let Value::Object(Some(elements)) = element_data {
                for i in 0..size.min(ctx.array_length(elements)) {
                    if let Value::Object(Some(url)) = ctx.get_array_element(elements, i) {
                        append_url(url);
                    }
                }
            }
        }
        // Only when slot 0 really holds a URL ARRAY. On a real `URLClassPath`
        // slot 0 is `path`, an ArrayList, and `array_length` of that is not a
        // URL count. Ask about the STASHED VALUE rather than the `ucp`'s
        // layout: `record_ucl_urls` stashes here precisely for REAL-layout
        // loaders (see its doc comment and the Tomcat StandardJarScanner
        // regression it names), so a real `ucp` carrying a real stash is an
        // expected shape and a synthetic-layout gate wrongly refuses it.
        if let Value::Object(Some(urls)) = ctx.get_field(ucp, UCP_STASHED_URLS) {
            if ucp_stash_is_reference_array(ctx, urls) {
                for i in 0..ctx.array_length(urls) {
                    if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                        append_url(url);
                    }
                }
            }
        }
    }

    out
}

/// Collect `http`/`https` base URL strings from a URLClassLoader's own
/// constructor URLs -- the network-classpath counterpart of
/// `loader_constructor_url_paths` (which only handles `file:`/`jar:` entries
/// resolvable as local filesystem paths).
pub fn loader_constructor_http_bases(ctx: &dyn NativeContext, loader: ObjectRef) -> Vec<String> {
    let mut out = Vec::new();

    // Both raw-slot families are gated on the layout that gives them their
    // meaning — see `cl_has_synthetic_layout` / `ucp_synthetic_layout`.
    if cl_has_synthetic_layout(ctx, loader) {
        if let Value::Object(Some(urls)) = ctx.get_field(loader, UCL_URLS_ARRAY) {
            let count = match ctx.get_field(loader, UCL_URL_COUNT) {
                Value::Int(n) if n > 0 => (n as usize).min(ctx.array_length(urls)),
                _ => 0,
            };
            for i in 0..count {
                if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                    if let Some(base) = http_base_from_url(ctx, url) {
                        out.push(base);
                    }
                }
            }
        }
    }

    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(loader, "ucp") {
        if ucp_synthetic_layout(ctx, ucp) {
            if let Value::Object(Some(urls)) = ctx.get_field(ucp, UCP_STASHED_URLS) {
                for i in 0..ctx.array_length(urls) {
                    if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                        if let Some(base) = http_base_from_url(ctx, url) {
                            out.push(base);
                        }
                    }
                }
            }
        }
    }

    out
}

/// Reconstruct an `http(s)://host[:port]/path` base string from a
/// `java.net.URL` object, or `None` if it is not an http/https URL. Reads
/// fields by NAME first (works for real-JDK URL objects, which carry genuine
/// JDK field names -- the same pattern `ucp_add_url`'s `"protocol"` read
/// uses), falling back to the synthetic numeric layout (`URL_FIELD_*`) so
/// synthetic-JDK mode's placeholder URL objects resolve too.
pub fn http_base_from_url(ctx: &dyn NativeContext, url_obj: ObjectRef) -> Option<String> {
    let read = |name: &str, idx: usize| -> Option<String> {
        match ctx.get_field_by_name(url_obj, name) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => match ctx.get_field(url_obj, idx) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            },
        }
    };
    let protocol = read("protocol", 0)?;
    if protocol != "http" && protocol != "https" {
        return None;
    }
    let host = read("host", 1).unwrap_or_default();
    let port = match ctx.get_field_by_name(url_obj, "port") {
        Value::Int(p) if p > 0 => Some(p),
        _ => match ctx.get_field(url_obj, 2) {
            Value::Int(p) if p > 0 => Some(p),
            _ => None,
        },
    };
    let path = read("path", 3).unwrap_or_default();

    let mut base = format!("{protocol}://{host}");
    if let Some(p) = port {
        base.push(':');
        base.push_str(&p.to_string());
    }
    base.push_str(&path);
    Some(base)
}

/// Attempt a real HTTP(S) GET for `resource_name` against each of `bases` in
/// turn (each a base URL collected by `loader_constructor_http_bases`, e.g.
/// `http://localhost:8500/test-classes/`). Returns the response body on the
/// first `200 OK`; returns `None` when every base yields a non-2xx status or
/// a connection failure -- a definitive "not found via this loader's own
/// classpath" the caller must NOT paper over with CratonVM's flat global
/// class store (see docs/feature-designs/native-dispatch-architecture.md -- Keycloak's
/// `TestClassServer` answers a non-permitted package with HTTP 403, which
/// must surface as `ClassNotFoundException`, not a silent success).
pub fn fetch_http_resource(
    ctx: &mut dyn NativeContext,
    bases: &[String],
    resource_name: &str,
) -> Option<Vec<u8>> {
    for base in bases {
        let sep = if base.ends_with('/') { "" } else { "/" };
        let uri = format!("{base}{sep}{resource_name}");
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let result = crate::http_client::perform_request(
            "GET",
            &uri,
            &[],
            &[],
            std::time::Duration::from_secs(10),
            false,
            5,
        );
        ctx.end_blocking_region();
        if let Ok(resp) = result {
            if resp.status == 200 {
                return Some(resp.body);            }        }    }
    None
}

/// True when `loader` is a `URLClassLoader`-shaped loader whose constructor URL
/// set we positively recorded (see [`record_ucl_urls`]) — i.e. its own URL view
/// is authoritative, and an EMPTY view genuinely means "this loader owns
/// nothing", not "we failed to observe its URLs".
///
/// The recorded set is either the synthetic-JDK `UCL_URLS_ARRAY` slot or the
/// real-JDK `ucp` placeholder our constructor shim installs; the presence of
/// EITHER is the proof. Same signal [`loader_local_resource_urls`] already
/// trusts for receiver-local resource lookup, so a loader this returns `false`
/// for is one whose local lookups are already going through the global path.
///
/// Requires an actual `java.net.URLClassLoader` (or subclass) receiver: those
/// raw slot indices only mean "URLs" on that shape. Probing them on, say, a
/// bare synthetic `java/lang/ClassLoader` reads an unrelated field — the same
/// out-of-bounds-slot hazard `is_user_defined_loader` guards against.
pub fn loader_has_recorded_url_set(ctx: &mut dyn NativeContext, loader: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(loader);
    let is_url_loader = ctx
        .class_id_by_name("java/net/URLClassLoader")
        .is_some_and(|ucl| cid == ucl || ctx.is_subclass(cid, ucl));
    if !is_url_loader {
        return false;
    }
    // The slot-4 disjunct is the SYNTHETIC signal and only means "URL array"
    // on the synthetic layout; on a real loader slot 4 is `parallelLockMap`, a
    // `ConcurrentHashMap` that is never null, so unguarded it answers `true`
    // for every real receiver without consulting anything about URLs. The
    // `ucp` disjunct is the real-layout signal and stands on its own.
    (cl_has_synthetic_layout(ctx, loader)
        && matches!(
            ctx.get_field(loader, UCL_URLS_ARRAY),
            Value::Object(Some(_))
        ))
        || matches!(ctx.get_field_by_name(loader, "ucp"), Value::Object(Some(_)))
}

/// Does `loader` answer resource lookups entirely out of a URL list CratonVM
/// can enumerate in full?
///
/// When this is true, `loader.getResources(name)` is exhaustive for that
/// loader — including when it comes back EMPTY — so a caller must not widen an
/// empty answer with a process-wide classpath scan. `ucl_find_resources` is
/// what makes that guarantee hold; this predicate is how a caller outside
/// `classloader.rs` (`service_loader::discover_providers`) asks whether it may
/// rely on it.
///
/// Deliberately narrower than [`loader_has_recorded_url_set`]: that one accepts
/// a `URLClassLoader` carrying a `ucp` whose URL list may not have been recorded
/// yet, which is precisely the "the local scan could not run" case a caller must
/// still fall back for.
pub fn loader_owns_complete_resource_view(
    ctx: &dyn NativeContext,
    loader: ObjectRef,
) -> bool {
    object_extends(ctx, loader, "java/net/URLClassLoader")
        && !loader_constructor_url_paths(ctx, loader).is_empty()
}

/// Is a package with class files under `class_glob` (e.g.
/// `com/example/pkg/*.class`) visible **to this specific loader**?
///
/// `ClassLoader.getDefinedPackage` is non-delegating in the JDK, so answering it
/// from the VM-global classpath makes every loader — including a deliberately
/// isolated one — claim every package on the process classpath. That is the
/// loader-identity gap behind
/// `fixed-suite-bugs/springboot/*-beandefinitionloader-package-scan-empty-FIXED.md`:
/// `new URLClassLoader("empty", new URL[0], null).getDefinedPackage(p)` answered
/// with a `Package` where HotSpot answers `null`.
///
/// Resolution order, deliberately failing back to the historical global answer
/// whenever the receiver's own view is not knowable:
/// 1. built-in loaders — see [`builtin_loader_segment`]. The APPLICATION and
///    PLATFORM loaders are probed against their OWN class-path segment, not the
///    concatenation; every other built-in (the boot loader) keeps the global
///    probe, because the boot loader genuinely does define the boot packages;
/// 2. a loader with its own recorded URLs — probe exactly those (already
///    pathing-jar aware via [`loader_local_resource_urls`]);
/// 3. a loader with a positively-recorded but EMPTY URL set — nothing is
///    visible, matching HotSpot;
/// 4. anything else (a custom loader we have no URL view of) — global probe,
///    i.e. unchanged from before this function existed.
/// Is this one of the loaders the VM itself creates (bootstrap / platform /
/// application), as opposed to a user-defined one?
///
/// **This is an identity test, not a claim about visibility.** Being built-in
/// does NOT mean the loader can see the whole process classpath — that reading
/// is exactly the bug `aa09d8bd8` fixed below, where the application loader
/// fabricated a `Package` for `java.lang`. Callers that want visibility must go
/// through [`package_class_files_visible_to_loader`], which segments it.
///
/// The one caller outside this module is `getDefinedPackage`'s DEFAULT-package
/// arm: a built-in loader that loaded a class from the classpath root has
/// defined the default package, and a user-defined loader has not.
pub fn loader_is_builtin(ctx: &mut dyn NativeContext, loader: ObjectRef) -> bool {
    // The shared name, not a fresh `String`: asked per call by the
    // `getDefinedPackage` Intrinsic's door (t1 wave 13, lane L71).
    ctx.class_name_arc_of_id(ctx.class_id_of_object(loader))
        .is_some_and(|n| {
            // A layer's `jdk.internal.loader.Loader` is a user loader
            // (`is_builtin_loader_class_in`; both modes since wave 48).
            (n.starts_with("jdk/internal/loader/") && &*n != JDK_LAYER_LOADER_CLASS)
                || n.starts_with("sun/misc/Launcher$")
        })
}

///
/// # 2026-08-22: step 1 used to be "built-in loaders ARE the global classpath"
///
/// It is not true of the application loader, and `RLangPackages` failed its
/// FIRST check on it, in BOTH modes:
///
/// ```text
///   appLoader.getDefinedPackage("java.lang")   HotSpot null   CratonVM java.lang
///   appLoader.getDefinedPackage("java.util")   HotSpot null   CratonVM java.util
///   platform .getDefinedPackage("java.lang")   HotSpot null   CratonVM java.lang
///   appLoader.getDefinedPackage("no.such")     HotSpot null   CratonVM null
/// ```
///
/// `java.lang` is defined by the BOOT loader. The global probe concatenates
/// bootstrap + extension + application, so the application loader found the
/// boot image's `java/lang/*.class` and fabricated a `Package` — the same
/// loader-identity error this function was written to fix for URLClassLoader,
/// left in place for the built-ins by the very branch that skipped them.
///
/// The `loader == None` arm above is untouched on purpose: it IS the boot
/// loader, and answering `true` for `java.lang` there is correct.
pub fn package_class_files_visible_to_loader(
    ctx: &mut dyn NativeContext,
    loader: Option<ObjectRef>,
    package_name: &str,
    class_glob: &str,
) -> bool {
    let Some(loader) = loader else {
        // No receiver object at all: this IS the boot loader, so ask the boot
        // loader's own definition question before falling back.
        if let Some(answer) =
            builtin_loader_defines_package(ctx, BuiltinLoaderKind::Boot, package_name)
        {
            return answer;
        }
        return !ctx.find_all_resource_urls(class_glob).is_empty();
    };
    let loader_class = ctx.class_name_of_id(ctx.class_id_of_object(loader));
    if loader_is_builtin(ctx, loader) {
        // The MODULE route first: for the boot and platform loaders it is the
        // only route there is. A class-path segment probe cannot answer for
        // them at all -- the boot image is a jimage, and a `java/sql/*.class`
        // glob over it returns nothing, which is why `java.sql` read `null` on
        // the platform loader and `java.lang` read `null` on the boot loader
        // (taking `Package.getPackage` down with it) until this arm existed.
        if let Some(kind) = builtin_loader_kind(loader_class.as_deref()) {
            if let Some(answer) = builtin_loader_defines_package(ctx, kind, package_name) {
                return answer;
            }
        }
        return match builtin_loader_segment(loader_class.as_deref()) {
            // The application loader's `-cp` segment, AND a loaded class. The
            // segment probe alone answers the visibility question this whole
            // function exists to stop answering: HotSpot's
            // `app.getDefinedPackage("com.example.app")` is `null` until a
            // class in it is defined and non-null after, and a class-file glob
            // cannot tell those two instants apart. Spring's
            // `BeanDefinitionLoader.findPackage` — the caller the app arm was
            // written for — loads a class from the package before asking
            // again, so it keeps working on the answer HotSpot gives it.
            Some(segment) => {
                !ctx.find_resource_urls_in_segment(class_glob, segment)
                    .is_empty()
                    && ctx.any_loaded_class_in_package(&package_name.replace('.', "/"))
            }
            // The boot loader, or a built-in shape this VM does not recognise:
            // the historical global probe, unchanged.
            None => !ctx.find_all_resource_urls(class_glob).is_empty(),
        };
    }
    if !loader_local_resource_urls(ctx, loader, class_glob).is_empty() {
        // `--real-jdk` (SBR3 `refmisc`): a class file in the loader's own URLs
        // proves the package is VISIBLE, not that the loader DEFINED it, and
        // `getDefinedPackage` answers HotSpot's `null` until a class of the
        // package is defined. The JDK's `URLClassLoader.defineClass` asks
        // exactly that before defining the first class of a `Sealed: true`
        // jar's package, and an eager `true` here made it throw "sealing
        // violation: can't seal package ...: already loaded".
        //
        // `--compatible` had the same failure (interpreter round i2 wave 64,
        // lane L5: probe `L5W64JarLoaderBytesIndex`'s sealed `a.jar`, in both
        // states of the lane's switch), and the rule is HotSpot's in both
        // modes, so it holds there too unless
        // `CRATONVM_COMPAT_URL_LOADER_DEFINED_PACKAGES=0`.
        if ctx.is_jdk_only() || ctx.compat_url_loader_defined_packages() {
            let ns = loader_namespace_id(ctx, loader);
            if ns >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED {
                return ctx
                    .any_loaded_class_in_package_for_loader(&package_name.replace('.', "/"), ns);
            }
        }
        return true;
    }
    if loader_has_recorded_url_set(ctx, loader) {
        return false;
    }
    // Step 4 -- a custom loader this VM has no URL view of -- was the LAST arm
    // still answering the visibility question this function exists to stop
    // answering. `new ClassLoader(null) {}`, which defines nothing at all,
    // claimed every application package on the process class path:
    //
    //   custom.getDefinedPackage("com.example.app")   HotSpot null   was: a Package
    //
    // Ask the loader's OWN definitions instead. A user-defined loader has a
    // namespace id of its own, and `any_loaded_class_in_package_for_loader`
    // answers exactly "did THIS loader define a class in this package" -- which
    // is `getDefinedPackage`'s contract, and is what the loaders this arm was
    // written for actually need:
    //
    // * ByteBuddy's `JavaDispatcher$DynamicClassLoader` asks about the package
    //   it has just defined `Invoker` into, so it still answers non-null;
    // * `GroovyClassLoader.definePackageInternal` reads
    //   `getDefinedPackage(p) == null` before `definePackage(p, ...)`. The
    //   FIRST class in a package answers null (as on HotSpot, and as the
    //   caller wants), the second answers non-null and the duplicate
    //   `definePackage` -- `IllegalArgumentException: <pkg>` -- is skipped.
    //
    // A loader with no namespace id of its own (id < 3: it delegates to the
    // built-in chain) keeps the historical global probe, unchanged.
    let namespace = loader_namespace_id(ctx, loader);
    if namespace >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED {
        return ctx
            .any_loaded_class_in_package_for_loader(&package_name.replace('.', "/"), namespace);
    }
    !ctx.find_all_resource_urls(class_glob).is_empty()
}

/// Which of the three built-in loaders this is, by class name.
///
/// Separate from [`builtin_loader_segment`] on purpose: that one answers
/// "which slice of the class path does this loader own", which is a question
/// only the application and platform loaders have an answer to, and it is the
/// WRONG question for a loader whose classes come out of a jimage.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuiltinLoaderKind {
    Boot,
    Platform,
    Application,
}

pub fn builtin_loader_kind(loader_class: Option<&str>) -> Option<BuiltinLoaderKind> {
    match loader_class? {
        "jdk/internal/loader/ClassLoaders$AppClassLoader" | "sun/misc/Launcher$AppClassLoader" => {
            Some(BuiltinLoaderKind::Application)
        }
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
        | "sun/misc/Launcher$ExtClassLoader" => Some(BuiltinLoaderKind::Platform),
        "jdk/internal/loader/ClassLoaders$BootClassLoader" => Some(BuiltinLoaderKind::Boot),
        _ => None,
    }
}

/// The JDK's own boot / platform module tables, read out of the running image.
///
/// `jdk.internal.module.ModuleLoaderMap$Modules.{bootModules,platformModules}`
/// are the two `Set<String>` statics the JDK's own module system consults to
/// decide which built-in loader defines a module's packages. Reading them beats
/// keeping a hand-copied list in Rust: the answer then comes from the image the
/// run actually loaded, and a JDK that moves a module between the two tables
/// moves this VM with it.
///
/// Memoised on SUCCESS ONLY. A failure -- the class not yet initialisable this
/// early in boot, a stripped or non-JDK image -- answers `None` and is retried
/// on the next call, which selects the historical class-path probe. That is the
/// pre-existing behaviour, never a silent "this loader defines nothing".
pub struct BuiltinModuleSets {
    pub boot: std::collections::HashSet<String>,
    pub platform: std::collections::HashSet<String>,
}

pub const MODULE_LOADER_MAP_MODULES: &str = "jdk/internal/module/ModuleLoaderMap$Modules";

thread_local! {
    /// Re-entrancy guard for [`jdk_builtin_module_sets`].
    ///
    /// Reading the tables runs Java: a class initialisation and three
    /// `invoke_virtual`s. If anything on that path reached
    /// `getDefinedPackage` again the cache would still be cold and the read
    /// would recurse without bound. The guard answers `None` on re-entry,
    /// which selects the historical class-path probe for that one inner call —
    /// the same fail-soft every other failure arm here takes.
    pub static READING_MODULE_SETS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn jdk_builtin_module_sets(ctx: &mut dyn NativeContext) -> Option<Arc<BuiltinModuleSets>> {
    // `OrderedPlMutex` at the leaf level, not a raw `Mutex`: this crate
    // re-enters the VM, so a global lock with no `LockLevel` is a deadlock the
    // order checker cannot see, and `lock_discipline_ratchet` refuses one
    // ("Do NOT raise the baseline").
    //
    // `Scratch` is honest here rather than convenient. The guard is never held
    // across anything: the read below runs Java -- a class initialisation and
    // three `invoke_virtual`s -- and it runs with NOTHING locked, because both
    // acquisitions are single statements that publish or fetch an `Arc` and
    // end. Nothing is taken while holding this, which is exactly what L0 means.
    //
    // Per VM (gc-common w10-f). The tables come out of the image THIS VM
    // loaded, and two VMs in one process need not load the same one; the
    // process-wide slot this replaced handed the first VM's tables to every
    // later VM. The VM's own `LoaderIdentityTables::module_sets` since
    // interpreter round i5 wave 102, lane L5 (a process map keyed by VM
    // identity before).
    //
    // Bound to a local FIRST. An `if let` scrutinee temporary lives to the end
    // of the whole `if let`, so a guard taken there would still be held in an
    // `else` arm the next edit adds.
    let hit = loader_tables(ctx).module_sets.lock().clone();
    if let Some(hit) = hit {
        return Some(hit);
    }
    if READING_MODULE_SETS.with(|f| f.replace(true)) {
        return None;
    }
    let sets = jdk_builtin_module_sets_uncached(ctx);
    READING_MODULE_SETS.with(|f| f.set(false));
    let sets = sets?;
    *loader_tables(ctx).module_sets.lock() = Some(Arc::clone(&sets));
    Some(sets)
}

/// Is `module` one the JDK's own table assigns to the PLATFORM loader, read
/// from [`jdk_builtin_module_sets`]' memo ONLY: `None` when this VM's tables
/// were not read yet (or cannot be). Runs no Java and takes one leaf lock, so
/// a message funnel reached while an exception is being raised can ask it
/// (interpreter round i1 wave 47, lane L4: HotSpot names a `java.sql` class's
/// loader `'platform'` in a `ClassCastException`; this VM defines it in the
/// boot namespace). [`warm_builtin_module_sets`] fills the memo at boot.
pub fn memoised_module_is_platform(tables: &LoaderIdentityTables, module: &str) -> Option<bool> {
    let sets = tables.module_sets.lock().clone()?;
    Some(sets.platform.contains(module))
}

/// The modules the JDK's table assigns to the PLATFORM loader, from
/// [`jdk_builtin_module_sets`]' memo only (`None` while it is cold). The boot
/// hands them to the class manager's module registry once
/// (`ModuleRegistry::set_platform_loader_modules`), so the access and
/// loader-constraint texts the classloading crate words can name such a
/// class's loader `'platform'` (interpreter round i1 wave 48, lane L5).
pub fn memoised_platform_modules(tables: &LoaderIdentityTables) -> Option<Vec<String>> {
    let sets = tables.module_sets.lock().clone()?;
    Some(sets.platform.iter().cloned().collect())
}

/// The modules the JDK's table assigns to the BOOTSTRAP loader, from the
/// same memo ([`memoised_platform_modules`]'s twin). The boot hands them to
/// the class manager's module registry with the platform set
/// (`ModuleRegistry::set_boot_loader_modules`), so the resolution door can
/// tell an application-loader image module (zt df4 §9c).
pub fn memoised_boot_modules(tables: &LoaderIdentityTables) -> Option<Vec<String>> {
    let sets = tables.module_sets.lock().clone()?;
    Some(sets.boot.iter().cloned().collect())
}

/// Read this VM's module tables into [`jdk_builtin_module_sets`]' memo, once
/// the module system is up, so [`memoised_module_is_platform`] has them. A
/// failed read leaves the memo cold (the reader then answers `None`, today's
/// answer). Idempotent: a warm memo returns at once.
pub fn warm_builtin_module_sets(ctx: &mut dyn NativeContext) -> bool {
    jdk_builtin_module_sets(ctx).is_some()
}

// (Interpreter round i5 wave 102, lane L5: the per-VM memos of the module
// tables and of each package's built-in loader -- `package -> the built-in
// loader the JDK's table gives its module`, wave 50 -- are fields of the VM's
// `LoaderIdentityTables` now, `module_sets` and `builtin_packages`; they were
// process maps keyed by VM identity, `builtin_module_sets_cache` and
// `platform_package_memo`.)

pub fn jdk_builtin_module_sets_uncached(ctx: &mut dyn NativeContext) -> Option<Arc<BuiltinModuleSets>> {
    let cid = ctx
        .ensure_class_initialized(MODULE_LOADER_MAP_MODULES)
        .ok()
        .or_else(|| ctx.class_id_by_name(MODULE_LOADER_MAP_MODULES))?;
    let boot = read_static_string_set(ctx, cid, "bootModules")?;
    let platform = read_static_string_set(ctx, cid, "platformModules")?;
    Some(Arc::new(BuiltinModuleSets { boot, platform }))
}

/// One `static final Set<String>` field, walked into a Rust set.
///
/// The set and its iterator are held as GLOBAL ROOTS across the `invoke_virtual`
/// calls rather than as raw `ObjectRef`s: `iterator()`/`next()` allocate, so a
/// collection can move both between one call and the next, and a moved receiver
/// is the shape that has cost this codebase whole sessions. Runs once per VM.
pub fn read_static_string_set(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    field: &str,
) -> Option<std::collections::HashSet<String>> {
    let index = ctx.static_field_index_by_name(class_id, field)?;
    let set = match ctx.get_static_field(class_id, index) {
        Value::Object(Some(set)) => set,
        _ => return None,
    };
    let set_root = ctx.add_global_root(set);
    let out = read_string_set_rooted(ctx, set_root);
    ctx.remove_global_root(set_root);
    out
}

pub fn read_string_set_rooted(
    ctx: &mut dyn NativeContext,
    set_root: usize,
) -> Option<std::collections::HashSet<String>> {
    let set = ctx.resolve_global_root(set_root)?;
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(set, "elements") {
        let len = ctx.array_length(arr);
        let mut out = std::collections::HashSet::with_capacity(len);
        for i in 0..len {
            if let Value::Object(Some(name)) = ctx.get_array_element(arr, i) {
                if let Some(s) = ctx.read_string(name) {
                    out.insert(s);
                }
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    let iterator = match ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        _ => return None,
    };
    let it_root = ctx.add_global_root(iterator);
    let mut out = std::collections::HashSet::new();
    // Bounded for the same reason `real_defined_package_names` is: a runaway
    // iterator must not hang a boot-time lookup. The JDK's two tables hold
    // ~50 names between them.
    for _ in 0..4096 {
        let Some(it) = ctx.resolve_global_root(it_root) else {
            break;
        };
        match ctx.invoke_virtual(it, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => break,
        }
        let Some(it) = ctx.resolve_global_root(it_root) else {
            break;
        };
        let Ok(Some(Value::Object(Some(name)))) =
            ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])
        else {
            break;
        };
        if let Some(name) = ctx.read_string(name) {
            out.insert(name);
        }
    }
    ctx.remove_global_root(it_root);
    // An EMPTY table is a failed read, not an answer: the JDK never ships one.
    // Reporting `None` keeps the caller on its historical probe instead of
    // freezing "nothing is defined" into the memo.
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Which built-in loader does the JDK's own table give the module of
/// `package_slash` (an image package)? `Platform` for the table's platform
/// modules, `Boot` for its boot modules, `Application` for any other module
/// of the image (`jdk.compiler`, `jdk.jshell`, ...: `ModuleLoaderMap` defines
/// every module in neither set to the application loader). `None` when the
/// tables or the package's module are not known yet.
///
/// Memoised per package. A package's module cannot change once the image is
/// loaded, and `Class.getClassLoader()` is asked far more often than there are
/// packages -- so the lookup is a short-string hash, not a module-registry walk
/// plus an `Arc` clone, on every call.
///
/// A package whose module is not yet known is NOT memoised: answering `false`
/// because the module registry had not been populated yet, and then freezing
/// it, is how a memo turns a boot-order accident into a permanent wrong answer.
/// Does the JDK's own table define the module of the image package
/// `package_slash` to the platform loader (`platform`) or to the application
/// loader (`!platform`)? A class of it carries the boot tag (0) in the class
/// store either way ([`package_builtin_kind`]; `false` while unknown).
pub fn image_package_defined_to(
    ctx: &mut dyn NativeContext,
    package_slash: &str,
    platform: bool,
) -> bool {
    match package_builtin_kind(ctx, package_slash) {
        Some(BuiltinLoaderKind::Platform) => platform,
        Some(BuiltinLoaderKind::Application) => !platform,
        _ => false,
    }
}

pub fn package_builtin_kind(
    ctx: &mut dyn NativeContext,
    package_slash: &str,
) -> Option<BuiltinLoaderKind> {
    // `OrderedPlMutex` at the leaf level, not a raw `Mutex`, for the same
    // reason as `jdk_builtin_module_sets` below: this crate re-enters the VM,
    // so a global lock with no `LockLevel` is a deadlock the order checker
    // cannot see, and `lock_discipline_ratchet` refuses one outright ("Do NOT
    // raise the baseline").
    //
    // `Scratch` is the honest level and not merely a convenient one: the guard
    // is never held across anything. The lookup below runs Java — module-set
    // construction plus `module_for_package` — and it runs with NOTHING
    // locked, because both acquisitions are single statements that read or
    // publish and end.
    //
    // Per VM since gc-common w10-f (`platform_package_memo`); the VM's own
    // `LoaderIdentityTables::builtin_packages` since i5 wave 102, lane L5.
    //
    // Bound to a local FIRST: an `if let` scrutinee temporary lives to the end
    // of the whole `if let`, so a guard taken there is still held in an `else`
    // arm the next edit adds.
    let hit = loader_tables(ctx)
        .builtin_packages
        .lock()
        .get(package_slash)
        .copied();
    if hit.is_some() {
        return hit;
    }
    let sets = jdk_builtin_module_sets(ctx)?;
    let module = ctx.module_for_package(package_slash)?;
    let answer = if sets.platform.contains(&module) {
        BuiltinLoaderKind::Platform
    } else if sets.boot.contains(&module) {
        BuiltinLoaderKind::Boot
    } else {
        BuiltinLoaderKind::Application
    };
    loader_tables(ctx)
        .builtin_packages
        .lock()
        .insert(package_slash.to_string(), answer);
    Some(answer)
}

/// Is `package_slash` in a module the JDK's own table assigns to the PLATFORM
/// loader? ([`package_builtin_kind`].)
pub fn package_is_platform_defined(ctx: &mut dyn NativeContext, package_slash: &str) -> bool {
    package_builtin_kind(ctx, package_slash) == Some(BuiltinLoaderKind::Platform)
}

/// The PLATFORM loader, when `class_name` (slash form) is an image class whose
/// module the JDK assigns to it; the APPLICATION loader for an image class of
/// a module the JDK gives that loader (`jdk.compiler`'s `JavacTool`), under
/// `CRATONVM_IMAGE_APP_MODULE_LOADERS` (interpreter round i1 wave 50, lane
/// L5); and `None` for a boot-module class, an application class, or an image
/// this VM cannot read the tables out of.
///
/// # Not every image class is boot-loaded
///
/// This VM reads the whole jimage through one class path and tags every class
/// in it `ClassLoaderId::Bootstrap`, so `Class.getClassLoader()` answered
/// `null` for all of them. The JDK does not: `ModuleLoaderMap` splits the
/// image's modules between the boot, platform and application loaders, and
/// the ~24 platform modules (`java.sql`, `java.net.http`, `java.scripting`,
/// `jdk.httpserver`, ...) are DEFINED by `ClassLoaders$PlatformClassLoader`,
/// the tool modules (`jdk.compiler`, `jdk.jshell`, `jdk.jartool`, ...) by
/// `ClassLoaders$AppClassLoader`. MEASURED:
///
/// ```text
///   java.sql.Connection .getClassLoader()   HotSpot PlatformClassLoader   was null
///   javax.script.ScriptEngine...            HotSpot PlatformClassLoader   was null
///   com.sun.tools.javac.api.JavacTool       HotSpot AppClassLoader        was null
///   java.lang.String    .getClassLoader()   HotSpot null                  null
///   java.awt.Color      .getClassLoader()   HotSpot null (java.desktop is BOOT)
/// ```
///
/// The direction matters the way it does for an application class reported as
/// bootstrap-loaded: `null` means "the boot loader owns this" to every caller
/// that keys a cache, picks a proxy loader, or decides a delegation parent.
///
/// This changes the REPORTED loader only. Class definition, resource
/// resolution and loader namespaces are untouched -- the class store stays
/// flat, and `is_builtin_loader_class` already keeps `Class.getResource*` off
/// the loader-delegation path for every built-in loader, the platform one
/// included.
pub fn builtin_loader_for_image_class(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Option<ObjectRef> {
    let (package_slash, _) = class_name.rsplit_once('/')?;
    match package_builtin_kind(ctx, package_slash)? {
        BuiltinLoaderKind::Platform => get_or_create_platform_loader(ctx).ok(),
        BuiltinLoaderKind::Application if ctx.image_app_module_loaders() => {
            get_or_create_app_loader(ctx).ok()
        }
        _ => None,
    }
}

/// The native loader id (`1` platform, `2` application) of the built-in
/// loader [`builtin_loader_for_image_class`] reports for the boot-namespace
/// image class `class_name`, or `None` when it reports the bootstrap loader.
/// Interpreter round i3 wave 79, lane L2: a captured frame's
/// `classLoaderName` and `format` follow the REPORTED loader, as HotSpot's
/// follow the defining one (`java.compiler` is the platform loader's).
pub fn image_class_builtin_loader_id(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Option<i32> {
    let (package_slash, _) = class_name.rsplit_once('/')?;
    match package_builtin_kind(ctx, package_slash)? {
        BuiltinLoaderKind::Platform => Some(1),
        BuiltinLoaderKind::Application if ctx.image_app_module_loaders() => Some(2),
        _ => None,
    }
}

/// zt t25/df11b (define page §9d): the loaded runtime-image class `internal_name`
/// of a PLATFORM module (`java/sql/Date`), which HotSpot's platform loader
/// defined and holds in its dictionary while CratonVM tags it boot (no
/// defining-loader row), so the `Extension` duplicate check never sees it. A
/// define of that name by the platform loader (its `defineClass`, or
/// `Lookup.defineClass` of a platform lookup class) is HotSpot's duplicate
/// `LinkageError`; CratonVM defined a second class (the loader's door) or met
/// the privileged-define forging guard (`SecurityException`, the lookup door).
/// `None`: an empty or non-image name, a boot-module or application-module
/// class, not loaded, the module tables not read yet (early boot), or
/// `CRATONVM_PLATFORM_IMAGE_DUPLICATE=0` (the positive control).
pub fn platform_image_class_loaded(
    ctx: &mut dyn NativeContext,
    internal_name: &str,
) -> Option<cratonvm_types::ClassId> {
    if internal_name.is_empty() || loader_tables(ctx).module_sets.lock().is_none() {
        return None;
    }
    let cid = ctx.class_id_by_name(internal_name)?;
    if ctx.loader_id_of_class(cid) != cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP as i32
        || defining_loader_for(loader_tables(ctx), cid.as_u32()).is_some()
    {
        return None;
    }
    if image_class_builtin_loader_id(ctx, internal_name)
        != Some(cratonvm_types::ClassLoaderId::NATIVE_EXTENSION as i32)
    {
        return None;
    }
    true
        .then_some(cid)
}

/// Is `class_name` (slash form) a runtime-image class of a module the JDK's
/// own table gives to the PLATFORM or APPLICATION loader (`java.sql`,
/// `jdk.compiler`), i.e. one the BOOTSTRAP loader does not define? `false`
/// for a boot-module class, a class outside the image, or an unreadable
/// table. `Class.forName(name, init, null)` asks it: HotSpot's bootstrap
/// search finds only boot-module classes (`Class.forName("java.sql.Date",
/// false, null)` is `ClassNotFoundException: java/sql/Date`), while the
/// package-prefix predicate (`is_bootstrap_class_name`) admits every
/// `java/` name (interpreter round i2 wave 54, lane L5; probe
/// `L5W54ImageModuleLoaders`). Only once the module tables are memoised
/// ([`warm_builtin_module_sets`], at boot once the module system is up): a
/// bootstrap lookup early in boot must not initialise `ModuleLoaderMap`, and
/// answers `false` (the prefix predicate alone, as before) while they are
/// cold. A package's first ask may still run `module_for_package`.
pub fn image_class_outside_boot_modules(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> bool {
    let Some((package_slash, _)) = class_name.rsplit_once('/') else {
        return false;
    };
    if loader_tables(ctx).module_sets.lock().is_none() {
        return false;
    }
    matches!(
        package_builtin_kind(ctx, package_slash),
        Some(BuiltinLoaderKind::Platform | BuiltinLoaderKind::Application)
    )
}

/// The `jrt:/<module>` code base HotSpot's built-in loaders give a class of a
/// non-boot image module (`jrt:/java.sql`, `jrt:/jdk.compiler`), under
/// `CRATONVM_IMAGE_APP_MODULE_LOADERS`; `None` for a boot module, a class
/// outside the image, or an unreadable table. This VM defines such a class
/// without a source and stamped it `file:/runtime-defined/<name>.class`
/// (interpreter round i1 wave 50, lane L5).
pub fn image_module_code_base(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Option<String> {
    if !ctx.image_app_module_loaders() {
        return None;
    }
    let (package_slash, _) = class_name.rsplit_once('/')?;
    match package_builtin_kind(ctx, package_slash)? {
        BuiltinLoaderKind::Boot => None,
        BuiltinLoaderKind::Platform | BuiltinLoaderKind::Application => {
            let module = ctx.module_for_package(package_slash)?;
            Some(format!("jrt:/{module}"))
        }
    }
}

/// The PLATFORM loader when `module_name` is one the JDK's own table assigns to
/// it; `None` for a boot module, an application module, or an unreadable image.
///
/// The module-name form of [`builtin_loader_for_image_class`], for
/// `Module.getClassLoader()`. The JDK keeps the two answers in step -- every
/// class in `java.sql` reports the same loader its module does -- so they have
/// to come from the same table or a caller can catch this VM contradicting
/// itself with two calls.
pub fn platform_loader_for_module(
    ctx: &mut dyn NativeContext,
    module_name: &str,
) -> Option<ObjectRef> {
    let sets = jdk_builtin_module_sets(ctx)?;
    if !sets.platform.contains(module_name) {
        // SBR2 wave 9 (modload), `--real-jdk`: every other module of the layer
        // (the `jdk.*` tool modules of the image and the `--module-path`
        // modules) is defined to the APPLICATION loader, as the JDK's own
        // `ModuleLoaderMap` assigns them. `Module.getClassLoader()` answered
        // `null` for them (HotSpot: the app loader). Not a module that only the
        // class path put in the registry: those are unnamed-module citizens.
        if builtin_loader_ordinal_of_module(ctx, module_name) == Some(2) {
            return get_or_create_app_loader(ctx).ok();
        }
        // Without the JDK's own built-in loaders (`--compatible`): the image's
        // APPLICATION modules (`jdk.compiler`, `jdk.jshell`, ...) answer the
        // application loader too, as every class in them already does
        // (`builtin_loader_for_image_class` under
        // `CRATONVM_IMAGE_APP_MODULE_LOADERS`): `ModuleLayer.boot()
        // .findLoader("jdk.compiler")` answered `null` there while
        // `JavacTool.class.getClassLoader()` answered the app loader
        // (interpreter round i2 wave 54, lane L5; probe
        // `L5W54ImageModuleLoaders`).
        if !real_builtin_loaders_enabled(ctx)
            && ctx.image_app_module_loaders()
            && !sets.boot.contains(module_name)
            && ctx.module_is_registered(module_name)
            && !ctx.module_is_class_path_only(module_name)
        {
            return get_or_create_app_loader(ctx).ok();
        }
        return None;
    }
    get_or_create_platform_loader(ctx).ok()
}

/// SBR2 wave 9 (modload), `--real-jdk` with the adopted built-in loaders: the
/// loader the JDK's own `ModuleLoaderMap` assigns `module_name` -- `0` boot,
/// `1` platform, `2` application (`NATIVE_APPLICATION`) -- or `None` for a name
/// the registry does not hold, a module only the class path put in it, or when
/// the loaders are not the JDK's own (the minted look-alikes define no modules).
pub fn builtin_loader_ordinal_of_module(
    ctx: &mut dyn NativeContext,
    module_name: &str,
) -> Option<u32> {
    if !real_builtin_loaders_enabled(ctx) || module_name.is_empty() {
        return None;
    }
    let sets = jdk_builtin_module_sets(ctx)?;
    if sets.boot.contains(module_name) {
        return Some(0);
    }
    if sets.platform.contains(module_name) {
        return Some(cratonvm_types::ClassLoaderId::NATIVE_EXTENSION);
    }
    if ctx.module_is_registered(module_name) && !ctx.module_is_class_path_only(module_name) {
        return Some(cratonvm_types::ClassLoaderId::NATIVE_APPLICATION);
    }
    None
}

/// SBR2 wave 9 (modload), `--real-jdk`: the modules `finish_boot_layer` defined
/// to the built-in loaders (`BuiltinClassLoader.loadModule`). Per VM: the
/// VM's own `LoaderIdentityTables::modules_defined_to_loaders` since
/// interpreter round i5 wave 102, lane L5 (a process map keyed by VM identity
/// before).
///
/// Record that this VM's boot modules are defined to the built-in loaders.
pub fn set_modules_defined_to_loaders(tables: &LoaderIdentityTables, on: bool) {
    tables.modules_defined_to_loaders.store(on, Ordering::Release);
}

pub fn modules_are_defined_to_loaders(tables: &LoaderIdentityTables) -> bool {
    tables.modules_defined_to_loaders.load(Ordering::Acquire)
}

/// SBR2 wave 9 (modload), `--real-jdk`: `findLoadedClass(name)` of a built-in
/// loader for a class of a module defined to THAT loader that the VM has not
/// materialised yet.
///
/// The real `BuiltinClassLoader.loadClassOrNull` / `findClass(String, String)`
/// bytecode asks `findLoadedClass` first and, for a package of a module defined
/// to the loader, otherwise reads the class bytes through the module's reader
/// and calls `defineClass`. This VM serves the image's and the module path's
/// classes from its flat store; defining a second copy from bytes (and reading
/// them through a `jrt:` reader) is exactly what must not happen. So the VM's
/// own class table answers: the class IS defined to its module's loader as soon
/// as it is asked for, and HotSpot's observable answers do not tell the
/// difference (`findLoadedClass` is `protected final`, reached only by a
/// subclass of a built-in loader, of which there is none).
///
/// Only for a module whose loader (per `ModuleLoaderMap`) is `this`; any other
/// loader answers `None` and the real code delegates to the one that is.
pub fn lazy_define_for_builtin_loader(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
) -> Option<ObjectRef> {
    if !ctx.is_jdk_only() || !modules_are_defined_to_loaders(loader_tables(ctx)) {
        return None;
    }
    let ordinal = builtin_loader_ordinal(ctx, this)?;
    let (pkg, _) = internal_name.rsplit_once('/')?;
    let module = ctx.module_for_package(pkg)?;
    if builtin_loader_ordinal_of_module(ctx, &module) != Some(ordinal) {
        return None;
    }
    match ctx.load_class(internal_name) {
        Ok(Some(Value::Object(Some(mirror)))) => {
            let cid = ctx.class_id_from_mirror(mirror)?;
            if ctx.loader_id_of_class(cid) > ordinal as i32 {
                return None;
            }
            Some(mirror)
        }
        _ => None,
    }
}

/// Is `loader` the VM's platform-loader singleton?
///
/// Identity first, class name as the real-JDK fallback -- the same two-step
/// [`loader_namespace_id_at`] uses, and for the same reason: the JDK can
/// manufacture another `PlatformClassLoader` object before our singleton is
/// observed.
pub fn is_platform_loader_object(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    platform_loader_of(loader_tables(ctx)).is_some_and(|p| p.as_ptr() == loader.as_ptr())
        || ctx
            .class_name_of_id(ctx.class_id_of_object(loader))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
}

/// Is `loader` the VM's application-loader singleton?
///
/// Identity first, class name as the real-JDK fallback -- the same two-step
/// [`is_platform_loader_object`] and [`builtin_loader_ordinal`] use.
pub fn is_app_loader_object(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    app_loader_of(loader_tables(ctx)).is_some_and(|p| p.as_ptr() == loader.as_ptr())
        || ctx
            .class_name_of_id(ctx.class_id_of_object(loader))
            .is_some_and(|name| {
                name == "jdk/internal/loader/ClassLoaders$AppClassLoader"
                    || name == "sun/misc/Launcher$AppClassLoader"
            })
}

/// Does this built-in loader DEFINE `package_name` (dot form)?
///
/// `None` means "not answerable here" -- the module tables could not be read, or
/// the application loader, whose packages come off the `-cp` segment and whose
/// existing probe is right. The caller turns `None` back into that probe.
///
/// Two conjuncts, and both are load-bearing:
///
/// * the package's module is in the JDK's own table for THIS loader. Module
///   membership alone is a capability, not a definition;
/// * a class in that package is actually LOADED. HotSpot defines a package when
///   a loader defines a class in it, so `plat.getDefinedPackage("java.sql")` is
///   `null` until something loads a `java.sql` class and non-null after --
///   which is what [`NativeContext::any_loaded_class_in_package`] answers.
///
/// A package in NO named module (the class path's unnamed module) is defined by
/// neither the boot nor the platform loader, so those two answer a definite
/// `false` rather than falling through to a global probe that would hand the
/// boot loader every application package on the class path.
pub fn builtin_loader_defines_package(
    ctx: &mut dyn NativeContext,
    kind: BuiltinLoaderKind,
    package_name: &str,
) -> Option<bool> {
    if kind == BuiltinLoaderKind::Application {
        return None;
    }
    let sets = jdk_builtin_module_sets(ctx)?;
    let slash = package_name.replace('.', "/");
    let table = match kind {
        BuiltinLoaderKind::Boot => &sets.boot,
        BuiltinLoaderKind::Platform => &sets.platform,
        BuiltinLoaderKind::Application => unreachable!("returned above"),
    };
    let in_table = ctx
        .module_for_package(&slash)
        .is_some_and(|module| table.contains(&module));
    if !in_table {
        return Some(false);
    }
    Some(ctx.any_loaded_class_in_package(&slash))
}

/// Which class-path segment a built-in loader OWNS, or `None` for the boot
/// loader (and any built-in shape not listed here).
///
/// Segment numbering is `ClassManager::find_resource_urls_in_segment`'s, which
/// is in turn `next_resource_url_from`'s: 0 bootstrap, 1 extension, 2
/// application.
///
/// `None` is the SAFE answer, not a gap: it selects the historical global
/// probe, so an unrecognised built-in behaves exactly as it did before this
/// function existed. Only the two loaders we can name positively are narrowed.
pub fn builtin_loader_segment(loader_class: Option<&str>) -> Option<u8> {
    match loader_class? {
        // The application loader owns `-cp` and nothing else.
        "jdk/internal/loader/ClassLoaders$AppClassLoader" | "sun/misc/Launcher$AppClassLoader" => {
            Some(2)
        }
        // The platform loader's extension segment, which is empty on a normal
        // run. It is reached only when the module tables above could NOT be
        // read: `builtin_loader_defines_package` answers for both the platform
        // and the boot loader before this match, and a segment probe cannot
        // answer for either of them anyway — the boot image is a jimage, and a
        // `java/sql/*.class` glob over it returns nothing.
        //
        // That was the residual the 2026-08-22 narrowing knowingly shipped
        // (`java.sql` read `null` on the platform loader), and it was wider
        // than the one package it named. Closed 2026-09-01 by the module route;
        // this arm is the fail-soft under it, not the answer.
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
        | "sun/misc/Launcher$ExtClassLoader" => Some(1),
        _ => None,
    }
}

pub fn loader_local_resource_urls(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    resource_name: &str,
) -> Vec<String> {
    let paths = loader_constructor_url_paths(ctx, loader);
    if paths.is_empty() {
        if crate::nbflags().dbg_uclres {
            eprintln!("[UCLRES-DBG] loader={loader:?} resource={resource_name} paths=[]");
        }
        return Vec::new();
    }
    // `cached_class_path_for_paths` (keyed by the exact paths vector) already
    // covers the repeated-rebuild cost that a "pathing JAR" -- a jar with no
    // class entries, only a manifest `Class-Path:` naming the real dependency
    // jars, used by e.g. the spring-boot-suite-runner to dodge Windows'
    // command-line length limit -- would otherwise pay on every single
    // lookup once `ClassPath::new` honours that manifest attribute (see
    // `classloading/src/class_path.rs`) and expands to the module's full,
    // possibly 100s-of-jars dependency list.
    // The asking loader's namespace id: a new loader re-checks the cached
    // row's archives on disk (interpreter round i4 wave 97, lane L5).
    // (Unused, and not computed, without a per-VM cache: mock contexts.)
    let asking = if archive_bytes_caches(&*ctx).is_some() {
        loader_namespace_id(ctx, loader)
    } else {
        0
    };
    let urls =
        cached_class_path_for_paths(&*ctx, &paths, asking).find_all_resource_urls(resource_name);
    if crate::nbflags().dbg_uclres {
        eprintln!(
            "[UCLRES-DBG] loader={loader:?} resource={resource_name} paths={paths:?} urls={urls:?}"
        );
    }
    urls
}

/// Per-(loader-namespace-id, class-name) locks serializing concurrent
/// `ucl_try_define_local_class` attempts for the SAME class through the SAME
/// `URLClassLoader` instance.
///
/// Without this, two threads racing to load the same not-yet-defined class
/// through the same loader (e.g. Spring Boot's
/// `OnClassCondition$ThreadedOutcomesResolver`, which evaluates
/// autoconfiguration conditions on a background thread pool while the main
/// thread needs the same framework classes through the same
/// `ModifiedClassPathClassLoader`) can both pass the "not yet defined" check
/// below before either calls `define_class_full`. The second call then hits
/// a genuine but spurious `IncompatibleClassChangeError`
/// ("already defined by <this> loader") wrapped as a `NoClassDefFoundError`
/// -- exactly the check-then-act race a real JVM's per-class
/// `getClassLoadingLock` exists to prevent. Keyed by loader namespace id
/// (not the loader `ObjectRef`, which the entry-point pin/GC dance below
/// makes awkward to hash) plus class name, so unrelated loaders defining a
/// same-named class concurrently are never serialized against each other.
pub fn url_classloader_define_locks() -> &'static Mutex<
    std::collections::HashMap<(u32, String), std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>>,
> {
    static INSTANCE: OnceLock<
        Mutex<
            std::collections::HashMap<
                (u32, String),
                std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>,
            >,
        >,
    > = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Removes a [`url_classloader_define_locks`] entry once the last thread
/// using it is done (gc-common w9-a).
///
/// The map used to keep one entry — a `String` plus an `Arc`'d mutex and
/// condvar — for every class ever defined through a `URLClassLoader`, in
/// every loader, in every VM of the process, forever: a per-define leak that
/// a webapp redeploy or a per-test loader grows by thousands of entries each
/// time. An entry is only needed while some thread is defining or waiting on
/// its `(namespace, name)` pair.
///
/// Must be declared AFTER the thread's own `Arc` clone (so it drops first,
/// while that clone still counts) and BEFORE the in-progress guard (so the
/// flag is cleared and waiters are woken before the map lock is taken).
/// Two strong references then mean "the map's and mine": nobody else holds
/// the entry, and nobody can take a new clone meanwhile, because clones are
/// only taken under the map lock this drop holds. A waiter that still holds
/// a clone keeps the entry, and removes it itself when it leaves. An entry
/// already replaced by a newer one for the same key is left alone.
pub struct DefineLockEntryRelease {
    pub key: (u32, String),
    pub lock_addr: usize,
}

impl Drop for DefineLockEntryRelease {
    fn drop(&mut self) {
        let mut locks = url_classloader_define_locks()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let unused = locks.get(&self.key).is_some_and(|entry| {
            std::sync::Arc::as_ptr(entry) as usize == self.lock_addr
                && std::sync::Arc::strong_count(entry) == 2
        });
        if unused {
            locks.remove(&self.key);
        }
        // Two threads leaving the same entry at once can each still see the
        // other's clone and both keep it; it is then held by the map alone.
        // New clones are only taken under this lock and keep the count at 2
        // or more, so a count of 1 means "abandoned". Sweep those too; with
        // the removal above the map holds only in-flight pairs, so this pass
        // is over a handful of entries.
        locks.retain(|_, entry| std::sync::Arc::strong_count(entry) > 1);
    }
}

/// Round 15 wave 3 hand-back (lane trace4): the `ClassNotFoundException` of
/// [`ucl_try_define_local_class`]'s authoritative miss. Its message is the
/// BINARY name (`r15.trace3.MissingClass`), as HotSpot's `findClass` /
/// `loadClass` throw it; this built it from the INTERNAL name
/// (`r15/trace3/MissingClass`), so an isolated `URLClassLoader(urls, null)`
/// miss under `--compatible` read differently from `--real-jdk` and HotSpot.
/// It is built as the served `loadClass`'s own misses are
/// (`classloader_real::load_class_miss_exception_of`), so a bare
/// `URLClassLoader`'s capture also lists `URLClassLoader.findClass` and the two
/// `ClassLoader.loadClass` frames (`stackwalker::served_loader_miss_frames`).
/// `CRATONVM_UCL_MISS_BINARY_NAME=0` restores the internal-name message
/// without the frames; read only on such a miss.
pub fn ucl_local_miss_exception(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal_name: &str,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let loader_class = ctx.class_id_of_object(loader);
    let binary_name = internal_name.replace('/', ".");
    crate::classloader_real::load_class_miss_exception_of(ctx, loader_class, &binary_name)
}

