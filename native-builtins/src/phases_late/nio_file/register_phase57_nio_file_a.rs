#[allow(unused_imports)]
use super::*;

pub fn register_phase57_nio_file_a(r: &mut NativeMethodRegistry) -> (usize, usize) {
    let path = "java/nio/file/Path";
    let _paths = "java/nio/file/Paths";
    let files = "java/nio/file/Files";
    let fs_class = "java/nio/file/FileSystem";
    let fsp = "java/nio/file/spi/FileSystemProvider";

    // 21 of the registrations this function makes on `path` own their slot
    // and stand in front of a `java/nio/file/Path` method that is `public
    // abstract` on every supported image -- `declared, no Code, not
    // ACC_NATIVE` in `--dump-native-registry --explain-jdk-only` terms.
    // `toString`/`equals` are `H14-1` Sec4's original two; the other nineteen
    // (`compareTo`, `endsWith`, `getFileName`, `getFileSystem`, `getName`,
    // `getNameCount`, `getParent`, `getRoot`, `hashCode`, `isAbsolute`,
    // `normalize`, `register`, `relativize`, `resolve`, `startsWith`,
    // `subpath`, `toAbsolutePath`, `toRealPath`, `toUri`) are counted, but
    // not individually adjudicated, by `H25-2` Sec1.2/N3: a retirement of any
    // one of them removes the only implementation `Path` has. Marked here,
    // not on each row, per that record's own N3.

    // --- Path extras ---
    reg_path! { r.register(path,
        "toAbsolutePath",
        "()Ljava/nio/file/Path;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut this);
            let abs = p57_absolute_path_string(&p);
            let result = p57_alloc_path(ctx, &abs)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_path! { r.register(path, "normalize", "()Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        let normalized = p57_normalize_path(&p);
        let result = p57_alloc_path(ctx, &normalized)?;
        Ok(Some(Value::Object(Some(result))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "startsWith", "(Ljava/lang/String;)Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let other_ref = obj_arg(args, 1)?;
        // The String argument is read BEFORE the Path, whose read can run Java
        // (gc-common w21-b).
        let other = ctx.read_string(other_ref).unwrap_or_default();
        let p = p57_read_path(ctx, &mut this);
        Ok(Some(Value::Int(if p.starts_with(&other) { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "endsWith", "(Ljava/lang/String;)Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let other_ref = obj_arg(args, 1)?;
        // The String argument is read BEFORE the Path, whose read can run Java
        // (gc-common w21-b).
        let other = ctx.read_string(other_ref).unwrap_or_default();
        let p = p57_read_path(ctx, &mut this);
        Ok(Some(Value::Int(if p.ends_with(&other) { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path,
        "startsWith",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let mut other = obj_arg(args, 1)?;
            let (p1, p2) = p57_read_path_pair(ctx, &mut this, &mut other);
            Ok(Some(Value::Int(if p1.starts_with(&p2) { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_path! { r.register(path, "endsWith", "(Ljava/nio/file/Path;)Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let mut other = obj_arg(args, 1)?;
        let (p1, p2) = p57_read_path_pair(ctx, &mut this, &mut other);
        Ok(Some(Value::Int(if p1.ends_with(&p2) { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "getNameCount", "()I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        let count = p57_name_elements(&p).len() as i32;
        Ok(Some(Value::Int(count)))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "getName", "(I)Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let idx = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let p = p57_read_path(ctx, &mut this);
        let parts: Vec<String> = p57_name_elements(&p);
        // FIX (finding 4): match the JDK — index < 0 or >= name count throws
        // IllegalArgumentException instead of silently returning an empty path.
        if idx < 0 || idx as usize >= parts.len() {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid index: {idx}"),
            }
            .into());
        }
        let name = parts[idx as usize].as_str();
        let result = p57_alloc_path(ctx, name)?;
        Ok(Some(Value::Object(Some(result))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "subpath", "(II)Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let begin = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let end = match args[2] {
            Value::Int(i) => i,
            _ => 0,
        };
        let p = p57_read_path(ctx, &mut this);
        let parts: Vec<String> = p57_name_elements(&p);
        let count = parts.len() as i32;
        // FIX (finding 4): match the JDK — beginIndex must be in [0,count),
        // endIndex in (beginIndex,count]; otherwise IllegalArgumentException.
        if begin < 0 || begin >= count || end <= begin || end > count {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid subpath range: begin={begin}, end={end}, count={count}"),
            }
            .into());
        }
        let sub: Vec<String> = parts[begin as usize..end as usize].to_vec();
        let result = p57_alloc_path(ctx, &sub.join("/"))?;
        Ok(Some(Value::Object(Some(result))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "toFile", "()Ljava/io/File;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        p57_path_to_file(ctx, &p)
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "toUri", "()Ljava/net/URI;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        let norm = p.replace('\\', "/");
        let abs = if norm.starts_with('/') {
            norm.clone()
        } else {
            format!("/{norm}")
        };
        // Percent-encode the path so chars like `#`/` `/`?` stay part of the path
        // (matches HotSpot's `Path.toUri()` — see the other `toUri` registration).
        // Per `UnixUriUtils.toUri`/`WindowsUriSupport.toUri`, a Path that
        // names an existing DIRECTORY renders with a trailing `/`; a file (or a
        // path that does not exist) does not. `java.io.File.toURI()` below
        // already applies the same rule. Until `Path` construction normalized
        // its stored string this was masked for paths the caller happened to
        // write with a trailing separator, and wrong for every other directory.
        let abs = if !abs.ends_with('/') && std::path::Path::new(&p).is_dir() {
            format!("{abs}/")
        } else {
            abs
        };
        let encoded = encode_file_uri_path(&abs);
        let uri_str = format!("file://{encoded}");
        let uri = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 7)?;
        // gc-common w15-f: the URI is rooted across the three raw-slot Strings
        // and `uri_publish_named` (which allocates a String per component and
        // returns `()`), and every store and the result go through its current
        // address. The second and third stores and the returned reference were
        // all the address `try_alloc` returned.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let uri_h = scope.root(uri);
        // JDK-ONLY-LAYOUT: raw slots only on OUR layout. On a real
        // `java.net.URI` these three are `scheme`, `fragment` and `host`, so a
        // `Path.toUri()` used to store its full text as the scheme, the literal
        // "file" as the fragment, and the path as the host.
        if crate::net_phase_e::uri_has_synthetic_layout(&*scope, uri) {
            for (slot, text) in [(0, uri_str.as_str()), (1, "file"), (4, encoded.as_str())] {
                let s = scope.create_string(text);
                let uri = scope.get(&uri_h);
                scope.set_field(uri, slot, Value::Object(Some(s)));
            }
        }
        // `encoded`, not `abs`: the override is the RAW path component, and
        // `uri_publish_named` derives `decodedPath` from it. Passing the decoded
        // spelling here is what made `getRawPath()` answer an un-escaped path
        // while `toString()` stayed correct -- see the note on that function.
        let uri = scope.get(&uri_h);
        crate::net_phase_e::uri_publish_named(&mut *scope, uri, &uri_str, Some(&encoded));
        let uri = scope.get(&uri_h);
        Ok(Some(Value::Object(Some(uri))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "compareTo", "(Ljava/nio/file/Path;)I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let mut other = obj_arg(args, 1)?;
        let (p1, p2) = p57_read_path_pair(ctx, &mut this, &mut other);
        Ok(Some(Value::Int(p1.cmp(&p2) as i32)))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "isAbsolute", "()Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        // keycloak-15: a drive-relative (`C:foo`) or driveless-rooted (`\foo`)
        // path is NOT absolute on Windows — see `p57_win_is_absolute`. On Unix
        // the POSIX rule (leading `/`) applies.
        let abs = if cfg!(windows) {
            p57_win_is_absolute(&p)
        } else {
            p.starts_with('/')
        };
        Ok(Some(Value::Int(if abs { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        let parts: Vec<String> = p57_name_elements(&p);
        use cratonvm_types::ArrayElementType;
        // gc-common w21-b: the array is rooted across every element Path's
        // allocation and the iterator's, and stored through its current
        // address (it was held raw across all of them). This registration is
        // overwritten by the later `iterator` below, which already did this.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let arr = scope.new_array(ArrayElementType::Reference, parts.len());
        let arr_h = scope.root(arr);
        for (i, part) in parts.iter().enumerate() {
            let path_obj = p57_alloc_path(&mut *scope, part)?;
            let arr = scope.get(&arr_h);
            scope.set_array_element(arr, i, Value::Object(Some(path_obj)));
        }
        let itr = try_alloc_concurrent_synthetic(&mut *scope, "Path$Itr", 2)?;
        let arr = scope.get(&arr_h);
        scope.set_field(itr, 0, Value::Object(Some(arr)));
        scope.set_field(itr, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(itr))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    // --- Paths.get extras ---
    // Already registered in earlier phase, but let's register Path.of (Java 11)
    //
    // S111r11 SB3: previously this implementation ignored the varargs and
    // returned just `first`. That broke `SystemModuleFinders.ofSystem()` which
    // calls `Path.of(javaHome, "lib", "modules")` — we returned `javaHome`,
    // `Files.isRegularFile(javaHome)` was false, and the JDK fell through to
    // `ModulePath.of(patcher, Path.of(javaHome, "modules"))` (which we also
    // collapsed to `javaHome`) → `ModulePath.scan` threw FindException
    // "Module format not recognized: <javaHome>". That FindException then
    // propagates out of `PathMatchingResourcePatternResolver.<clinit>` (Spring
    // calls `ModuleFinder.ofSystem().findAll()` at line 216). Fix: walk the
    // varargs array and resolve each component onto the running path.
    reg_path! { r.register(path,
        "of",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            let first_ref = obj_arg(args, 0)?;
            let first = ctx.read_string(first_ref).unwrap_or_default();
            let mut acc = first;
            if let Some(Value::Object(Some(arr))) = args.get(1) {
                let len = ctx.array_length(*arr);
                // K5 (round 15 wave 6, lane jni6): read only when there is a
                // `more` part.
                let join = len > 0;
                let mut literal = (cfg!(windows) && join).then(|| acc.clone());
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                        let part = ctx.read_string(s).unwrap_or_default();
                        if let Some(literal) = literal.as_mut() {
                            p57_windows_literal_join(literal, &part);
                        }
                        if join {
                            p57_get_path_join(&mut acc, &part);
                        } else if !part.is_empty() {
                            acc = p57_resolve_paths(&acc, &part);
                        }
                    }
                }
                if let Some((input, reason, index)) = literal
                    .as_deref()
                    .and_then(p57_windows_join_refusal)
                    .filter(|_| {
                        true
                    })
                {
                    return p57_throw_invalid_path(ctx, &input, &reason, index);
                }
            }
            // Real Path constructors drop trailing name separators (e.g.
            // Path.of("spring/") -> "spring"); the synthetic Path kept them, so
            // `toAbsolutePath()` ("…/spring/") != `normalize()` ("…/spring")
            // (Spring Boot buildpack ZipFileTarArchive/ImageBuildpack
            // "Malformed zip entry name"). Strip them here, preserving roots
            // ("/", "C:/") and jar: filesystem paths.
            if !acc.starts_with("jar:") && !acc.contains("!/") {
                while acc.len() > 1 && acc.ends_with('/') && !acc[..acc.len() - 1].ends_with(':') {
                    acc.truncate(acc.len() - 1);
                }
            }
            // Round 15 wave 5 (lane jdkonly7): `Path.of` refuses what
            // `Paths.get` refuses (w4a: `R15Jdkonly4StaleBuilders` K7,
            // `Path.of("a\u0000b")` returned a Path under `--compatible`).
            if let Some((input, reason, index)) = p57_path_of_refusal(&acc) {
                return p57_throw_invalid_path(ctx, &input, &reason, index);
            }
            let result = p57_alloc_path(ctx, &acc)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // Path.of(URI) — Spring Boot 3.4 fat-jar launcher uses
    // `Path.of(URL.toURI()).toFile()` to convert a `file:/C:/...jar` URL
    // back into a `File`. Read the URI's `path` field via `get_field_by_name`
    // (real-JDK URI declares 16+ fields; using by-name access keeps us
    // independent of layout drift). Fall back to URI's `string` (full URI
    // text) and strip the `file:` scheme prefix when path is unset.
    reg_path! { r.register(path,
        "of",
        "(Ljava/net/URI;)Ljava/nio/file/Path;",
        |ctx, args| {
            let uri = obj_arg(args, 0)?;
            let uri_text = p57_uri_full_text(ctx, uri);
            // THE SCHEME DECIDES, and both refusals were missing.
            // `Path.of(URI)` is `Paths.get(uri)`: a URI with no scheme is not
            // absolute and raises `IllegalArgumentException`, and a scheme no
            // installed provider claims raises `FileSystemNotFoundException`.
            // This VM answered a Path for both — so `Path.of(new URI(
            // "http://example.com/x"))` handed back a relative filesystem path
            // built out of a URL, which then resolved against the CWD.
            //
            // MEASURED with `L4CensusTail.java`. `file:` and `jar:`
            // are the two this VM serves; anything else is a refusal rather
            // than a guess.
            {
                let head = uri_text.split('/').next().unwrap_or("");
                let scheme = match head.split_once(':') {
                    Some((s, _))
                        if !s.is_empty()
                            && s.chars().all(|c| {
                                c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'
                            })
                            && s.starts_with(|c: char| c.is_ascii_alphabetic()) =>
                    {
                        s.to_ascii_lowercase()
                    }
                    _ => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: format!("URI is not absolute: {uri_text}"),
                        }
                        .into())
                    }
                };
                // `--real-jdk`: every scheme but `file` belongs to the REAL
                // installed providers -- the archive-provider wave retired
                // `installedProviders`, so `jar:` is `jdk.nio.zipfs` and
                // `jrt:` is `jdk.internal.jrtfs`. Do what `Path.of(URI)`'s own
                // bytecode does for such a scheme and hand the URI to the
                // first installed provider that claims it, instead of minting
                // this VM's synthetic archive path: that path belonged to no
                // real filesystem, so `Paths.get(jar:...)` answered outside the
                // `ZipFileSystem` the caller had just mounted, and an unmounted
                // archive answered a path where HotSpot throws
                // `FileSystemNotFoundException`. `file:` stays this native's.
                if (ctx.is_jdk_only()
                    || ctx.aligned(cratonvm_types::HotSpotAlignment::JdkArchiveFs))
                    && scheme != "file"
                {
                    return path_of_uri_through_installed_providers(ctx, uri, &scheme);
                }
                if scheme != "file" && scheme != "jar" {
                    return Err(p57_file_system_not_found(ctx, &scheme));
                }
            }
            // `Paths.get(jar:file:...!/entry)` is the resource-facing half of
            // the jar-FS contract. Jetty's PathResourceFactory mounts the URI
            // first, then calls this conversion for the root and every
            // resolved child. Treating the full `jar:` text as an ordinary host
            // path loses the mounted archive identity, making Files.isDirectory
            // false and Files.list() empty despite a valid central directory.
            if let Some((jar, entry)) = p57_jar_uri_to_entry_path(&uri_text) {
                // gc-common w21-b: the FileSystem is rooted across the Path's
                // allocation and stored through its current address.
                let fs = p57_alloc_jar_filesystem(ctx, &jar)?;
                let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                let fs_h = scope.root(fs);
                let result = p57_alloc_path(&mut *scope, &jarfs_encode(&jar, &entry))?;
                let fs = scope.get(&fs_h);
                scope.set_field(result, p57_path_slots_for(scope.vm_identity()).fs, Value::Object(Some(fs)));
                return Ok(Some(Value::Object(Some(result))));
            }
            // Opaque file-scheme URIs (`file:.`, `file:foo`) are not
            // hierarchical: the real JDK throws here rather than yielding a
            // path. Match that so callers like Spring's PathEditor fall back
            // to their resource mechanism.
            if p57_uri_is_opaque_file(&uri_text) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "URI is not hierarchical".to_string(),
                }
                .into());
            }
            // Resolve the URI's filesystem path. Our URI synthetic has been
            // populated by `url_parse` (URL.toURI), which writes by INDEX —
            // not by name — into slots 0..5. The real-JDK `URI` field
            // layout differs from that index map (URI.path lives at slot 6,
            // URI.string at slot 18), so `get_field_by_name(uri, "path")`
            // can return null or stale slots. Read defensively from
            // multiple sources and strip any `file:` scheme prefix before
            // handing the result to `p57_to_os_path`.
            let mut candidates: Vec<String> = Vec::new();
            // 0. `URI.getPath()` -- the DECODED path component, which is what
            //    the real `Path.of(URI)` ends up with. Every field probe below
            //    reads a RAW (still percent-encoded) component, so a directory
            //    genuinely named `custom#root` came back as `custom%23root`
            //    and `Files.exists`/`Files.walk` saw nothing
            //    (`core.io.support.PathMatchingResourcePatternResolverTests
            //    .encodedHashtagInPath`). Same shape as the `new File(URI)`
            //    raw-path fix.
            //
            //    gen r4w3/rooting: `getPath()` is a GC point and the probes
            //    below read `uri` again -- through the pinned address.
            let uri_pin = ctx.pin_native_root(uri);
            let decoded_path = ctx.invoke_virtual(uri, "getPath", "()Ljava/lang/String;", &[]);
            let uri = ctx.read_native_pin(uri_pin, uri);
            ctx.unpin_native_roots(uri_pin);
            if let Ok(Some(Value::Object(Some(s)))) = decoded_path {
                if let Some(decoded) = ctx.read_string(s) {
                    candidates.push(decoded);
                }
            }
            // 1. URI.path by name (real-JDK URIs constructed via real-JDK
            //    URI bytecode populate this; ours don't but cheap to try).
            if let Value::Object(Some(s)) = ctx.get_field_by_name(uri, "path") {
                if let Some(t) = ctx.read_string(s) {
                    candidates.push(t);
                }
            }
            // 2. Synthetic URL_FIELD_PATH (slot 3) where url_parse stores
            //    the post-scheme path for `file:` URIs.
            if let Value::Object(Some(s)) = ctx.get_field(uri, 3) {
                if let Some(t) = ctx.read_string(s) {
                    candidates.push(t);
                }
            }
            // 3. Slot 4 — the `Path.toUri()` natives' synthetic layout
            //    stores the path component there (URIs from
            //    `selectClasspathRoots`-style Path→URI→Path round trips,
            //    e.g. the JUnit5 ClasspathScanner). Without this probe the
            //    round trip yielded an EMPTY path: by-name `path`/`string`
            //    resolve to real-JDK URI slots (6/18) that are out of
            //    bounds on the 5-slot synthetic, and slots 3/5 are unset.
            if let Value::Object(Some(s)) = ctx.get_field(uri, 4) {
                if let Some(t) = ctx.read_string(s) {
                    candidates.push(t);
                }
            }
            // 4. URI.string by name (full URI text on real-JDK URIs).
            if let Value::Object(Some(s)) = ctx.get_field_by_name(uri, "string") {
                if let Some(t) = ctx.read_string(s) {
                    candidates.push(t);
                }
            }
            // 5. Synthetic URL_FIELD_FULL (slot 5) where url_parse stores
            //    the full URI string. (For our synthetic URIs allocated
            //    with real-JDK layout this slot may have been clobbered
            //    with non-string data, so guarded by Object pattern.)
            if let Value::Object(Some(s)) = ctx.get_field(uri, 5) {
                if let Some(t) = ctx.read_string(s) {
                    candidates.push(t);
                }
            }
            // 6. Slot 0 — the `Path.toUri()` synthetic layout stores the
            //    full `file://...` text there; the scheme-strip loop below
            //    reduces it to the path component.
            if let Value::Object(Some(s)) = ctx.get_field(uri, 0) {
                if let Some(t) = ctx.read_string(s) {
                    candidates.push(t);
                }
            }
            // Pick the first non-empty candidate; strip any `file:` scheme.
            let mut path_str = String::new();
            for c in candidates {
                if c.is_empty() {
                    continue;
                }
                let stripped = c
                    .strip_prefix("file://")
                    .or_else(|| c.strip_prefix("file:"))
                    .map(|s| s.to_string())
                    .unwrap_or(c);
                // Reject candidates that still look like a URI (contain
                // ':' before the path's drive letter) — those are stale.
                if stripped.contains("file:") {
                    continue;
                }
                path_str = stripped;
                break;
            }
            let os_path = p57_to_os_path(&path_str);
            if crate::nbflags().dbg_sbload {
                eprintln!("[DBG_SBLOAD] Path.of(URI) -> {:?}", os_path);
            }
            let result = p57_alloc_path(ctx, &os_path)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // --- Path.getFileSystem() → the FileSystem this Path belongs to ---
    // FileSystem = 2-field synthetic (field 0 = separator String).
    // If the Path was produced by `FileSystem.getPath`, return that exact
    // FileSystem object (stored in the `fs` slot) so identity checks
    // hold; otherwise fall back to a default FileSystem.
    reg_path! { r.register(path,
        "getFileSystem",
        "()Ljava/nio/file/FileSystem;",
        |ctx, args| {
            if let Ok(mut this) = obj_arg(args, 0) {
                if let Value::Object(Some(fs)) = ctx.get_field(this, p57_path_slots_for(ctx.vm_identity()).fs) {
                    return Ok(Some(Value::Object(Some(fs))));
                }
                // Encoded virtual-FS paths produced during a walk carry no owning
                // FileSystem; reconstruct one so `path.getFileSystem().provider()
                // .getScheme()` reports jrt/jar (javac inspects this).
                let p = p57_read_path(ctx, &mut this);
                if let Some((jh, _)) = jrtfs_decode(&p) {
                    let fs = p57_alloc_jrt_filesystem(ctx, &jh)?;
                    return Ok(Some(Value::Object(Some(fs))));
                }
                if let Some((jar, _)) = jarfs_decode(&p) {
                    let fs = p57_alloc_default_filesystem(ctx)?;
                    // gc-common w18-f: the fresh FileSystem is rooted across
                    // the String's allocation and re-read for the store (it
                    // was written, and returned, at its pre-allocation
                    // address).
                    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                    let fs_h = scope.root(fs);
                    let jp = scope.create_string(&jar);
                    let fs = scope.get(&fs_h);
                    scope.set_field(fs, P57_FS_JAR_FIELD, Value::Object(Some(jp)));
                    return Ok(Some(Value::Object(Some(fs))));
                }
            }
            // No owning FileSystem recorded: this is a default-filesystem
            // path, so return THE default-FS singleton (identity checks
            // against FileSystems.getDefault() must hold).
            let fs = p57_default_filesystem_singleton(ctx)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // --- Path.resolve(String) → Path ---
    reg_path! { r.register(path,
        "resolve",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let other_ref = obj_arg(args, 1)?;
            // The String argument is read BEFORE the Path, whose read can run
            // Java (gc-common w21-b).
            let other = ctx.read_string(other_ref).unwrap_or_default();
            let base = p57_read_path(ctx, &mut this);
            let resolved = p57_resolve_paths(&base, &other);
            let result = p57_alloc_path(ctx, &resolved)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // --- Path.resolve(Path) → Path ---
    reg_path! { r.register(path,
        "resolve",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        p57_path_resolve_path, cratonvm_native_api::NativeKind::Bridge,
    ) }
    // Pulled into a free function (same reason as `p57_path_get_file_name`
    // above: a captureless closure and a named `fn` are interchangeable
    // under `NativeCallback`'s bare-`fn`-pointer type) so the
    // `sun/nio/fs/WindowsPath.resolve` registration below can call the
    // identical, already jar/jrt-aware body instead of a second copy.
    fn p57_path_resolve_path(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        let mut other = obj_arg(args, 1)?;
        let (base, other_str) = p57_read_path_pair(ctx, &mut this, &mut other);
        let resolved = p57_resolve_paths(&base, &other_str);
        let result = p57_alloc_path(ctx, &resolved)?;
        Ok(Some(Value::Object(Some(result))))
    }

    // `sun/nio/fs/WindowsPath.resolve(Path)` is a retired shadow
    // (`RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES`), so real bytecode wins for
    // any real-class-stamped receiver — jar/jrt Paths included. Real
    // `resolve(Path)` (javap-verified) builds the joined path by directly
    // string-concatenating `this.path` and `other.path` with a `\` — the
    // RAW, still-sentinel-encoded strings `path_layout.rs`'s `string` slot
    // holds for a jar/jrt Path (see `p57_path_get_file_name`'s registration
    // comment above for why: nothing decodes them for real bytecode, which
    // has no notion of the encoding at all), producing a mangled path that
    // is neither Path's real entry, and whose lookups then
    // `NoSuchFileException`.
    //
    // Found via real in-process javac's `RelativeDirectory.resolveAgainst`
    // (`RelativePath.java`, `lib/src.zip`-read): `directory.resolve(path
    // .replace("/", sep))` — the default `Path.resolve(String)` method
    // (itself a retired INTERFACE-level shadow, `javap`-verified:
    // `getFileSystem().getPath(other)` then `this.resolve(thatPath)`) lands
    // here for the actual join. `JavacFileManager.DirectoryContainer.list`
    // calls this once per package looked up during type resolution — HIB-CV-27's
    // jrt filesystem correctly lists `/modules`, `initSystemModules` correctly
    // discovers each module's location (`p57_path_get_file_name`'s fix,
    // above), but resolving `<module-location>/java/lang` to look up the
    // `java.lang` package's contents mangled here, so `Symtab`'s bootstrap
    // symbol completion found no `java.lang` package to import.
    //
    // Same `register_with_kind(..., Intrinsic)` shape as this file's other
    // fixes for the same collision family: a plain `register` on a retired
    // triple is auto-downgraded to `cratonvm_native_api::NativeKind::SyntheticStub` (REJECTED
    // under `--real-jdk`), so there is no active registration for dispatch
    // to prefer without it. Receiver-aware on EITHER operand — `this` OR
    // `other` decoding as jar/jrt is enough to know real bytecode's
    // string-concat body cannot be trusted (an absolute `other` short-circuits
    // to `other` on HotSpot too — real bytecode gets that part right — but a
    // relative `other` reaches the concatenation this fixes) — reusing the
    // identical `p57_path_resolve_path` body; any other pair is handed
    // straight to its own real bytecode via `invoke_virtual_bytecode_only`,
    // so this changes nothing for two real host `WindowsPath`s.
    register_vfs_windows_path_intrinsic(
        r,
        "resolve",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            // gc-common w21-b: both reads can run Java (a foreign Path's
            // `toString()`), so `this` / `other` are handed back current and
            // the delegation below gets them, not the `args` snapshot.
            let mut this = obj_arg(args, 0)?;
            let mut other = obj_arg(args, 1)?;
            let (this_p, other_p) = p57_read_path_pair(ctx, &mut this, &mut other);
            if vfs_decode(&this_p).is_some() || vfs_decode(&other_p).is_some() {
                return p57_path_resolve_path(
                    ctx,
                    &[Value::Object(Some(this)), Value::Object(Some(other))],
                );
            }
            ctx.invoke_virtual_bytecode_only(
                this,
                "resolve",
                "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
                &[Value::Object(Some(other))],
            )
        },
    );

    // --- Path.resolveSibling(String) → Path ---
    reg_path! { r.register(path,
        "resolveSibling",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let other_ref = obj_arg(args, 1)?;
            // The String argument is read BEFORE the Path, whose read can run
            // Java (gc-common w21-b).
            let other = ctx.read_string(other_ref).unwrap_or_default();
            let base = p57_read_path(ctx, &mut this);
            let parent = p57_parent_of(&base);
            let resolved = p57_resolve_paths(&parent, &other);
            let result = p57_alloc_path(ctx, &resolved)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // --- Path.resolveSibling(Path) → Path ---
    reg_path! { r.register(path,
        "resolveSibling",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let mut other = obj_arg(args, 1)?;
            let (base, other_str) = p57_read_path_pair(ctx, &mut this, &mut other);
            let parent = p57_parent_of(&base);
            let resolved = p57_resolve_paths(&parent, &other_str);
            let result = p57_alloc_path(ctx, &resolved)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // --- Path.relativize(Path) → Path ---
    reg_path! { r.register(path,
        "relativize",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let mut other = obj_arg(args, 1)?;
            let (base, target) = p57_read_path_pair(ctx, &mut this, &mut other);
            // Virtual-FS (jar / jrt): relativize within the entry namespace and
            // return a PLAIN relative path (NOT re-encoded). javac's
            // `ArchiveContainer` keys its package map on
            // `root.relativize(dir).toString()` (e.g. "org/h2/tools"), and
            // JUnit5's ClasspathScanner does `relativize(...).toString()
            // .replace(fs.getSeparator(), ".")`. The previous version ran
            // `std::path::strip_prefix` on the SENTINEL-ENCODED strings, which is
            // component-based and could not strip the shared
            // `…<jar>\u{1}` prefix (the jar-filename+sentinel component never
            // matched), so it returned the whole encoded target as garbage —
            // javac then keyed packages on garbage and reported "package
            // org.h2.tools does not exist" for every classpath jar.
            if let (Some((_, _, be)), Some((_, _, te))) = (vfs_decode(&base), vfs_decode(&target)) {
                let rel = p57_relativize(&be, &te).unwrap_or(te);
                // gc-common w21-b: the owning FileSystem is read, and rooted,
                // BEFORE the result's allocation. It was read through `this`
                // after it -- a pre-allocation address.
                let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                let fs_h = match scope.get_field(this, p57_path_slots_for(scope.vm_identity()).fs) {
                    Value::Object(Some(fs)) => Some(scope.root(fs)),
                    _ => None,
                };
                let result = p57_alloc_path(&mut *scope, &rel)?;
                // Tag the result with the source's owning virtual FS so its
                // `toString()` renders with '/' (the zipfs/jrtfs separator), not
                // the host '\' — javac keys its package map on
                // `root.relativize(dir).toString()`.
                if let Some(h) = &fs_h {
                    let fs = scope.get(h);
                    scope.set_field(result, p57_path_slots_for(scope.vm_identity()).fs, Value::Object(Some(fs)));
                }
                return Ok(Some(Value::Object(Some(result))));
            }
            // Host paths: compute the real relative path (with `..` backtracking)
            // off the shared root, not just a forward strip_prefix. Falls back to
            // `target` when the roots differ (can't be relativized).
            //
            // ...except that the JDK does NOT fall back there, it THROWS.
            // `Path.relativize` is specified to raise IllegalArgumentException
            // when "this path and the given path do not have the same root",
            // and the plainest case of that is one path absolute and the other
            // relative. MEASURED: `Paths.get("/x").relativize(Paths.get("y"))`
            // returned `y` here against HotSpot's IllegalArgumentException, in
            // both modes (`probes/TailFamilySweep.java`).
            //
            // Handing back the TARGET is the dangerous shape: the caller
            // believes it holds a path relative to `base`, and resolving it
            // against `base` yields something that was never asked for. A throw
            // is the JDK's answer and is loud.
            //
            // The predicate is the ROOT, not absoluteness. `WindowsPath
            // .relativize` opens `if (!this.getRoot().equals(other.getRoot()))
            // throw`, and on Windows those are different questions: a
            // driveless-rooted `\x` HAS a root and is NOT absolute --
            // `p57_win_is_absolute` says so deliberately, for keycloak-15. A
            // first version of this fix compared absoluteness and did not fire
            // for `Paths.get("/x").relativize(Paths.get("y"))`, which is the
            // very row that found the defect.
            //
            // Deliberately narrow: only a ROOT MISMATCH throws. The virtual-FS
            // arm above keeps its fallback untouched -- javac's
            // ArchiveContainer and JUnit5's ClasspathScanner depend on it, and
            // that arm's roots are the jar sentinel, not a filesystem root.
            let root_of = |s: &str| -> Option<String> {
                if cfg!(windows) {
                    p57_parse_win_root(s).0
                } else if s.starts_with('/') {
                    Some("/".to_string())
                } else {
                    None
                }
            };
            if root_of(&base) != root_of(&target) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "'other' is different type of Path".to_string(),
                }
                .into());
            }
            let relative = p57_relativize(&base, &target).unwrap_or_else(|| target.clone());
            let result = p57_alloc_path(ctx, &relative)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // --- Path.getParent() → Path (or null) ---
    reg_path! { r.register(path, "getParent", "()Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        let parent = p57_parent_of(&p);
        if parent.is_empty() {
            Ok(Some(Value::Object(None)))
        } else {
            let result = p57_alloc_path(ctx, &parent)?;
            Ok(Some(Value::Object(Some(result))))
        }
    }, cratonvm_native_api::NativeKind::Bridge) }

    // --- Path.getRoot() → Path (or null) ---
    reg_path! { r.register(path, "getRoot", "()Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        // keycloak-15: explicit Windows drive/UNC root parsing (the stored string
        // is '/'-canonical, so the old `[2]==b'\\'` check never matched a drive).
        match p57_parse_root(&p).0 {
            Some(root) => {
                let result = p57_alloc_path(ctx, &root)?;
                Ok(Some(Value::Object(Some(result))))
            }
            None => Ok(Some(Value::Object(None))),
        }
    }, cratonvm_native_api::NativeKind::Bridge) }

    // --- Path.getFileName() → Path (or null) ---
    // Real JDK: returns null when the path has no file component (e.g. "/").
    // CratonVM divergence: smallrye-config 3.16's
    // AbstractLocationConfigSourceLoader$ConfigSourceClassPathConsumer.accept(Path)
    // immediately invokes .toString() on the returned Path and assumes it is
    // non-null.  When our synthetic Path holds a normalized string like
    // "/" or "" (e.g. forward-slash inputs that std::path::Path::file_name
    // refuses to split on Windows), returning null triggers a "Cannot invoke
    // toString on null" NPE that aborts Keycloak 26 boot.  Fall back to a
    // manual basename via the last path separator so the consumer's
    // validExtension() check rejects it cleanly instead of NPEing.
    reg_path! { r.register(path, "getFileName", "()Ljava/nio/file/Path;", p57_path_get_file_name, cratonvm_native_api::NativeKind::Bridge) }
    // The body used to be inline here as `path`'s registration closure.
    // Pulled into a free function (`NativeCallback` is a bare `fn` pointer,
    // so a captureless closure and a named function are interchangeable)
    // so `sun/nio/fs/WindowsPath.getFileName`'s registration below — needed
    // because that concrete triple is a retired shadow and shadows THIS one
    // for any real-class-stamped receiver, jar/jrt Paths included since
    // 2026-09-16 — can call the identical, already-correct, already-jar/jrt-
    // aware body instead of a second copy that could drift from it. See that
    // registration's own comment for the collision this closes.
    fn p57_path_get_file_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        {
            let mut this = obj_arg(args, 0)?;
            let p_raw = p57_read_path(ctx, &mut this);
            let p = vfs_decode(&p_raw)
                .map(|(_, _, e)| e)
                .unwrap_or_else(|| p_raw.clone());
            let name = std::path::Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .or_else(|| {
                    // Manual basename: take the substring after the last '/' or '\\'.
                    let trimmed = p.trim_end_matches(['/', '\\']);
                    if trimmed.is_empty() {
                        None
                    } else {
                        let idx = trimmed
                            .rfind(|c: char| c == '/' || c == '\\')
                            .map(|i| i + 1)
                            .unwrap_or(0);
                        Some(trimmed[idx..].to_string())
                    }
                });
            match name {
                Some(n) => {
                    let result = p57_alloc_path(ctx, &n)?;
                    Ok(Some(Value::Object(Some(result))))
                }
                None => {
                    // A path with no name element is a ROOT, and `getFileName()`
                    // is null there. The EMPTY path is a different thing — it
                    // has ONE name element, the empty one, and answers a
                    // non-null empty Path — and this arm cannot tell them apart
                    // on its own, so it asks `p57_parse_root`: a root is
                    // `(Some(_), [])`.
                    //
                    // The non-null "" used to be returned for BOTH, with the
                    // comment below arguing for it on the strength of one
                    // consumer. MEASURED against HotSpot:
                    //
                    //   Paths.get("/").getFileName()    null    this VM ""
                    //   Paths.get("//").getFileName()   null    this VM ""
                    //   Paths.get("").getFileName()     ""      this VM ""   (agreed)
                    //
                    // The empty-path row is why the fix is a root TEST and not
                    // a blanket null: returning null for both would break the
                    // row that was already right.
                    //
                    // On the consumer named below: a caller that NPEs on a null
                    // `getFileName()` for "/" would NPE on HotSpot too, so the
                    // non-null "" was hiding a different defect — whatever hands
                    // that consumer a root path — rather than fixing one.
                    let (root, names) = p57_parse_root(&p);
                    if root.is_some() && names.is_empty() {
                        return Ok(Some(Value::Object(None)));
                    }
                    // A path with no name element is a root. The JDK contract is
                    // `getFileName() == null` here, and javac's
                    // `JavacFileManager$ArchiveContainer.preVisitDirectory` relies
                    // on it: `Path name = dir.getFileName(); if (name != null &&
                    // !SourceVersion.isName(name.toString())) SKIP_SUBTREE`. For a
                    // jar/jrt root a non-null "" makes `isName("")` false, so the
                    // walker SKIP_SUBTREEs the whole archive at its root — the
                    // classpath jar (and the runtime image) go completely
                    // unindexed and every type resolves to "package X does not
                    // exist" (broke in-process javac / H2 CREATE ALIAS). Return
                    // null for an encoded virtual-FS root. HOST paths keep the
                    // legacy non-null "" (smallrye-config's Keycloak path consumer
                    // NPEs on a null getFileName for "/").
                    if vfs_decode(&p_raw).is_some() {
                        return Ok(Some(Value::Object(None)));
                    }
                    let result = p57_alloc_path(ctx, "")?;
                    Ok(Some(Value::Object(Some(result))))
                }
            }
        }
    }

    // `sun/nio/fs/WindowsPath.getFileName` (the SAME descriptor as the
    // interface registration just above — real `WindowsPath` never declares
    // a covariant override for this one, `javap`-verified) is a retired
    // shadow (`RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES`), so real bytecode
    // wins for any real-class-stamped receiver — jar/jrt Paths included,
    // since 2026-09-16 stamps every Path this VM mints as the one real
    // carrier class. Real `getFileName()` reads `this.path`/`this.offsets`
    // directly; `offsets` is left unset for a jar/jrt Path (nothing
    // pre-populates it for the sentinel-encoded string `path_layout.rs`'s
    // `stored`/`string` slot actually holds — the tag+java_home+entry
    // encoding `jarfs_encode`/`jrtfs_encode` produce, not a plain '/'-joined
    // name), so real bytecode's null-guarded lazy `initOffsets()` never runs
    // either, and with no offsets it returns THIS VM's OWN stored string
    // whole — the raw, still-encoded sentinel text, tag and java_home and
    // all — instead of just the final logical segment.
    //
    // Found via real in-process javac's `SystemModulesLocationHandler
    // .initSystemModules`: `String moduleName = entry.getFileName()
    // .toString();` (real `Locations.java`, `lib/src.zip`-read, not guessed)
    // for each `/modules/<name>` directory entry HIB-CV-27's jrt filesystem
    // lists — every module then registered under the raw encoded string as
    // its "name" instead of e.g. `"jdk.jshell"`, so `Modules.getUnnamedModuleCompleter`'s
    // module-by-name lookups (`java.base` included) never matched anything
    // and platform-class resolution failed package lookup entirely.
    //
    // Same `register_with_kind(..., Intrinsic)` shape as the `toRealPath` fix
    // just below `toAbsolutePath` above, and for the identical reason: a
    // plain `register` on a retired triple is auto-downgraded to
    // `cratonvm_native_api::NativeKind::SyntheticStub` (REJECTED under `--real-jdk`) by
    // `NativeMethodRegistry::register`'s own `triple_is_retired_shadow`
    // check, so there would be no active registration for dispatch to prefer
    // at all. Receiver-aware: a jar/jrt Path reuses the identical,
    // already-correct `p57_path_get_file_name` body just defined; any other
    // receiver is handed straight to its own real bytecode via
    // `invoke_virtual_bytecode_only`, so this changes nothing for a real
    // host `WindowsPath`.
    register_vfs_windows_path_intrinsic(r, "getFileName", "()Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        if vfs_decode(&p).is_some() {
            return p57_path_get_file_name(ctx, &[Value::Object(Some(this))]);
        }
        ctx.invoke_virtual_bytecode_only(this, "getFileName", "()Ljava/nio/file/Path;", &[])
    });

    // --- Path.toString() → String ---
    // Rendering: `p57_path_slot_display_string` (jar/jrt entries with '/',
    // a virtual-FS relative path with '/', a Windows host path in the JDK's
    // own stored `\` form, else the OS-native separator).
    reg_path! { r.register(path, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let display = p57_path_slot_display_string(ctx, this);
        let s = ctx.create_string(&display);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    // --- Path.equals(Object) → boolean ---
    reg_path! { r.register(path, "equals", "(Ljava/lang/Object;)Z", p57_path_equals, cratonvm_native_api::NativeKind::Bridge) }
    // Pulled into a free function for the same reason as
    // `p57_path_get_file_name`/`p57_path_resolve_path` above: the
    // `sun/nio/fs/WindowsPath.equals`/`.hashCode` registrations below need
    // the identical body. Comparing the RAW stored strings (not decoded
    // entries) is correct as-is for a jar/jrt receiver -- `jarfs_encode`/
    // `jrtfs_encode` are deterministic, so two Paths naming the same jrt/jar
    // entry already carry the identical raw string, with no need to decode
    // first.
    fn p57_path_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        match args.get(1).copied() {
            // `UnixPath.equals` is `ob instanceof UnixPath p && compareTo(p)
            // == 0`; the type test was missing entirely here, so a `String`
            // with the same text compared EQUAL. See `p57_is_path_object`.
            //
            // gc-common w21-b: the type test runs first (no Java), then both
            // texts are read together, so `other` is not asked through its
            // address from before `this`'s read.
            Some(Value::Object(Some(mut other))) if p57_is_path_object(ctx, other) => {
                let (p1, p2) = p57_read_path_pair(ctx, &mut this, &mut other);
                Ok(Some(Value::Int(i32::from(p1 == p2))))
            }
            _ => Ok(Some(Value::Int(0))),
        }
    }

    // --- Path.hashCode() → int ---
    reg_path! { r.register(path, "hashCode", "()I", p57_path_hash_code, cratonvm_native_api::NativeKind::Bridge) }
    fn p57_path_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        // Simple hash matching Java's hashCode() contract
        let mut h: i32 = 0;
        for b in p.bytes() {
            h = h.wrapping_mul(31).wrapping_add(b as i32);
        }
        Ok(Some(Value::Int(h)))
    }

    // `sun/nio/fs/WindowsPath.equals`/`.hashCode` are BOTH retired shadows
    // (`RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES`), so real bytecode wins for
    // any real-class-stamped receiver — jar/jrt Paths included. Real
    // `equals()`/`hashCode()` read `this.path` directly, the RAW still-
    // sentinel-encoded string for a jar/jrt Path (same root cause as every
    // other fix in this family), so two Paths naming the identical jrt/jar
    // entry can come back unequal or differently-hashed depending on
    // incidental field noise real bytecode was never written to ignore.
    //
    // Found via real in-process javac's own `JavacFileManager.containers`
    // (`JavacFileManager.java`, `lib/src.zip`-read): a `HashMap<Path,
    // Container>` keyed on exactly this pair. `ClassFinder.fillIn`
    // (`msym.complete()`, reading `java.base`'s own `module-info.class` byte
    // for byte) and `Modules.initModules()` both call `getContainer` on
    // module root Paths well before `TypeEnter` ever runs — with `equals`/
    // `hashCode` unreliable, the map either MISSES its own earlier entry (an
    // extra, harmless re-scan) or, worse, HITS a stale/unrelated entry cached
    // under a colliding key and returns the WRONG `Container` for a later
    // lookup on the identical logical path. Confirmed empirically: `fm.list`
    // on `java_base.classLocation` here returns real 299-file counts,
    // `PathFileObject.forDirectoryPath`'s own `DirectoryFileObject` is what
    // `DirectoryContainer.list` mints (`javap`-verified) — but the actual
    // objects observed at this point in the real compile are
    // `PathFileObject$SimpleFileObject`, which only `DirectoryContainer
    // .getFileObject` (a single-name lookup, not a listing) and a couple of
    // unrelated call sites ever construct. That mismatch is the map handing
    // back a container this path never should have gotten, and
    // `SimpleFileObject.inferBinaryName` — which walks the location's search
    // paths calling `Path.startsWith` (ALSO retired, ALSO reading raw
    // fields) looking for a prefix match — finds none against a container it
    // was never built for, so it returns the empty string. `ClassFinder
    // .fillIn`'s `SourceVersion.isIdentifier("")` is `false`, so every file
    // is silently dropped and the package completes with zero members.
    //
    // Same `register_with_kind(..., Intrinsic)` shape as this file's other
    // fixes for the same family — a plain `register` on a retired triple is
    // auto-downgraded to `cratonvm_native_api::NativeKind::SyntheticStub` (REJECTED under
    // `--real-jdk`). Receiver-aware: EITHER operand decoding as jar/jrt is
    // enough (an `equals`/`hashCode` comparison spanning a jar/jrt Path and a
    // real one is never equal either way, and both routes agree on that);
    // any other pair is handed straight to its own real bytecode via
    // `invoke_virtual_bytecode_only`, so this changes nothing for two real
    // host `WindowsPath`s.
    register_vfs_windows_path_intrinsic(r, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        // gc-common w21-b: the reads hand `this` / `other` back current, and
        // both the delegation and the bytecode call get those, not `args`.
        let mut this = obj_arg(args, 0)?;
        let (this_p, other_is_vfs, other) = match args.get(1).copied() {
            Some(Value::Object(Some(mut o))) => {
                let (tp, op) = p57_read_path_pair(ctx, &mut this, &mut o);
                (tp, vfs_decode(&op).is_some(), Value::Object(Some(o)))
            }
            other => (
                p57_read_path(ctx, &mut this),
                false,
                other.unwrap_or(Value::Object(None)),
            ),
        };
        if vfs_decode(&this_p).is_some() || other_is_vfs {
            return p57_path_equals(ctx, &[Value::Object(Some(this)), other]);
        }
        let other = match other {
            Value::Object(Some(o)) => o,
            _ => obj_arg(args, 1)?,
        };
        ctx.invoke_virtual_bytecode_only(
            this,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(other))],
        )
    });
    register_vfs_windows_path_intrinsic(r, "hashCode", "()I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        if vfs_decode(&p).is_some() {
            return p57_path_hash_code(ctx, &[Value::Object(Some(this))]);
        }
        ctx.invoke_virtual_bytecode_only(this, "hashCode", "()I", &[])
    });

    // --- FileSystems.getDefault() → FileSystem ---
    let file_systems = "java/nio/file/FileSystems";
    r.register(
        file_systems,
        "getDefault",
        "()Ljava/nio/file/FileSystem;",
        |ctx, _args| {
            let fs = p57_default_filesystem_singleton(ctx)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- sun.nio.fs.DefaultFileSystemProvider.theFileSystem() → FileSystem ---
    //
    // The SECOND door to the same singleton. `getDefault()` above was
    // intercepted and this one was not, so the two disagreed: the real
    // `DefaultFileSystemProvider.<clinit>` ran and built its own
    // `WindowsFileSystem`, and the `jdk_concrete_getclass_alias` mapping
    // (FileSystem -> sun.nio.fs.WindowsFileSystem) made both objects report the
    // same class name, which is what hid the split.
    //
    // This is NOT a niche door. Verbatim callers in java.base (JDK 25 src.zip):
    //   * java.util.zip.ZipFile$Source.builtInFS  -- static final, then
    //     `Files.readAttributes(builtInFS.getPath(file.getPath()), ...)` on
    //     EVERY zip/jar open.
    //   * java.io.FilePermission.builtInFS -- static final, plus the derived
    //     `here` / EMPTY_PATH / DASH_PATH / DOTDOT_PATH constants.
    //   * java.io.WinNTFileSystem.isInvalid() -- the Windows `File` path
    //     validity check.
    //   * java.nio.file.FileSystems.getDefault() itself, on the
    //     `!VM.isModuleSystemInited()` branch -- i.e. for the whole of early
    //     boot, `getDefault()` IS `theFileSystem()`.
    //   * jdk.internal.jimage.ImageReaderFactory.<clinit>, reflectively.
    //
    // Every `Path` the second filesystem minted was a real-bytecode
    // `WindowsPath` whose field 0 is a `WindowsFileSystem` OBJECT (javap: the
    // first instance field is `private final WindowsFileSystem fs`; the String
    // `path` is field 3). `p57_read_path` reads field 0, `read_string`
    // correctly refuses a non-String, the documented `toString()` fallback
    // re-enters this same native because dispatch is receiver-aware, and the
    // `IN_TOSTRING_FALLBACK` guard then returns "". Downstream every file-IO
    // native saw an EMPTY path -- the empty-message `NoSuchFileException` that
    // kills `System.initPhase2`.
    //
    // HotSpot 25 returns one shared object through both doors, measured:
    // `theFileSystem==getDefault|true` (scratchpad/c14/FsIdentity.java). The
    // mutation control (FsIdentityMutant.java) builds a second provider by hand
    // and that row flips to false, so the row is load-bearing rather than
    // vacuously green.
    //
    // Descriptor read off the image, not recalled -- `javap -p --module
    // java.base sun.nio.fs.DefaultFileSystemProvider` on Temurin 25.0.3.9:
    //   public static java.nio.file.FileSystem theFileSystem();
    // It returns the INTERFACE type on every platform, so this descriptor is
    // portable. Static, so `args` carries no receiver.
    // docs/feature-designs/native-dispatch-architecture.md
    r.register(
        "sun/nio/fs/DefaultFileSystemProvider",
        "theFileSystem",
        "()Ljava/nio/file/FileSystem;",
        |ctx, _args| {
            let fs = p57_default_filesystem_singleton(ctx)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- sun.nio.fs.DefaultFileSystemProvider.instance() → the provider ---
    //
    // Registered TOGETHER with `theFileSystem` above and with the
    // `FileSystem.provider()` / `installedProviders()` repairs below, because
    // this door is only half a fix on its own: `sun.nio.ch.UnixDomainSockets.
    // generateTempName()` compares
    //   path.getFileSystem().provider() != DefaultFileSystemProvider.instance()
    // with `!=`, so unifying one side while the other still mints a fresh
    // object per call leaves the comparison exactly as broken as before.
    //
    // Leaving it unregistered is not neutral either: `instance()` compiles to a
    // bare `getstatic INSTANCE`, and that getstatic triggers
    // `DefaultFileSystemProvider.<clinit>`, whose whole body is
    // `INSTANCE = new WindowsFileSystemProvider()` -- and
    // `WindowsFileSystemProvider.<init>` does
    // `theFileSystem = new WindowsFileSystem(this, StaticProperty.userDir())`.
    // So an un-intercepted `instance()` reconstructs the very second filesystem
    // the registration above exists to eliminate.
    // `java.nio.file.FileSystems$DefaultFileSystemHolder.getDefaultProvider()`
    // calls it directly.
    //
    // PLATFORM-SPECIFIC DESCRIPTOR. Unlike `theFileSystem`, `instance()`
    // returns the CONCRETE provider type, which differs per platform. The
    // Windows arm is verified by javap on Temurin 25.0.3.9 windows/x64:
    //   public static sun.nio.fs.WindowsFileSystemProvider instance();
    // The other two arms are UNVERIFIED -- javap the corresponding image before
    // trusting them. The failure mode if an arm is wrong is a registration that
    // never matches (a dead registration), which is today's behaviour, not a
    // crash; that is why guessing here is acceptable and guessing in the
    // returned VALUE would not be.
    let instance_desc: &'static str = if cfg!(windows) {
        "()Lsun/nio/fs/WindowsFileSystemProvider;"
    } else if cfg!(target_os = "macos") {
        "()Lsun/nio/fs/MacOSXFileSystemProvider;"
    } else {
        "()Lsun/nio/fs/LinuxFileSystemProvider;"
    };
    r.register(
        "sun/nio/fs/DefaultFileSystemProvider",
        "instance",
        instance_desc,
        |ctx, _args| {
            let p = p57_default_provider_singleton(ctx)?;
            Ok(Some(Value::Object(Some(p))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // A NULL `env` MAP IS AN NPE, not an empty environment. Every provider the
    // JDK consults dereferences the map (`ZipFileSystem` reads its options out
    // of it before it will even look at the file), so `newFileSystem(path,
    // null)` fails before the path is examined. This VM ignored the argument
    // entirely and handed back a jar filesystem over whatever the path was --
    // for a path that is not an archive at all, a filesystem whose every later
    // operation is a puzzle at a site far from the mistake.
    //
    // MEASURED with `L4TailSweep2.java`. The non-null case is
    // deliberately UNCHANGED: what these overloads should do with a valid
    // environment over a non-archive file is `ProviderNotFoundException`, and
    // that is a wider change than this lane has a measurement for -- the jar
    // loading paths reach these three registrations constantly. Recorded as an
    // observation, not fixed.
    r.register(
        file_systems,
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;Ljava/lang/ClassLoader;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            let jar_path = obj_arg(args, 0)
                .ok()
                .map(|mut p| p57_read_path(ctx, &mut p))
                .unwrap_or_default();
            if let Some(refused) = p57_refuse_non_archive(ctx, &jar_path) {
                return Err(refused);
            }
            let fs = p57_alloc_jar_filesystem(ctx, &jar_path)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        file_systems,
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            let jar_path = obj_arg(args, 0)
                .ok()
                .map(|mut p| p57_read_path(ctx, &mut p))
                .unwrap_or_default();
            if let Some(refused) = p57_refuse_non_archive(ctx, &jar_path) {
                return Err(refused);
            }
            let fs = p57_alloc_jar_filesystem(ctx, &jar_path)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        file_systems,
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/lang/ClassLoader;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            let jar_path = obj_arg(args, 0)
                .ok()
                .map(|mut p| p57_read_path(ctx, &mut p))
                .unwrap_or_default();
            if let Some(refused) = p57_refuse_non_archive(ctx, &jar_path) {
                return Err(refused);
            }
            let fs = p57_alloc_jar_filesystem(ctx, &jar_path)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- FileSystem methods ---
    // Captured before ANY `java/nio/file/FileSystem` registration in this
    // function -- there are two such blocks (`fs_class` here, `fsys` later),
    // and `mirror_class_registrations` filters by class name over the WHOLE
    // registration dump since `since`, not by contiguity, so one count taken
    // here and one mirror call at the end of this function (after both
    // blocks) covers both.
    let __fs_rows_before = r.dump_registrations().len();
    let fs_class = "java/nio/file/FileSystem";

    r.register(
        fs_class,
        "getSeparator",
        "()Ljava/lang/String;",
        |ctx, args| {
            // A mounted-jar FileSystem renders '/' (matches the JDK zipfs
            // separator — JUnit5's ClasspathScanner splits scanned entry paths
            // on this to build package names); the host FS renders the OS
            // separator.
            //
            // EXCEPT on Windows, where `p57_write_path_fields` now stores a
            // vfs Path's JAVA-VISIBLE string `\`-delimited (real
            // `WindowsPath.getFileName`/`.getParent` scan `path` for a
            // literal `\` independent of `offsets` — see that function's own
            // comment). `vfs_decode` undoes the conversion for every
            // Rust-side reader, but `getSeparator()` is not one of those
            // readers: it answers from `p57_fs_is_virtual` alone, so it kept
            // claiming `/` while the string it is describing now actually
            // uses `\`. Real bytecode that combines the two — `SimpleFileObject
            // .inferBinaryName`'s `toBinaryName`, `relativePath.toString()
            // .replace(sep, ".")` — silently found nothing to replace and
            // handed back a binary name with the separators still literally
            // in it (`"java\lang\AbstractMethodError"`, no dots), which is
            // not a valid identifier, so `ClassFinder.fillIn` dropped the
            // file. Found via real in-process javac's own package-content
            // listing (`JavacFileManager$DirectoryContainer`/
            // `PathFileObject$SimpleFileObject`), reached once
            // `SystemModulesLocationHandler.initSystemModules` and the six
            // `WindowsPath` fixes in this file get module discovery working.
            // See docs/feature-designs/native-dispatch-architecture.md
            // h2-inprocess-javac-jrt-modules-listing-gap-FIXED-20260923.md (its "Related").
            if let Ok(this) = obj_arg(args, 0) {
                if p57_fs_is_virtual(ctx, this) {
                    let sep = if cfg!(windows) { "\\" } else { "/" };
                    let s = ctx.create_string(sep);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            let sep = if cfg!(windows) { "\\" } else { "/" };
            let s = ctx.create_string(sep);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        fs_class,
        "getPath",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            // arg 0 = this FileSystem; arg 1 = first path element;
            // arg 2 = remaining elements (String[]).
            let this = obj_arg(args, 0)?;
            let first_ref = obj_arg(args, 1)?;
            let mut first = ctx.read_string(first_ref).unwrap_or_default();
            // Append the varargs elements with the jar separator.
            if let Ok(rest) = obj_arg(args, 2) {
                let len = ctx.array_length(rest);
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(rest, i) {
                        let part = ctx.read_string(s).unwrap_or_default();
                        if !part.is_empty() {
                            if !first.is_empty() && !first.ends_with('/') {
                                first.push('/');
                            }
                            first.push_str(&part);
                        }
                    }
                }
            }
            // If this FileSystem was mounted from a JAR or is the runtime image
            // (jrt:), produce an encoded virtual-FS path so the file-IO natives
            // resolve it against the archive / jimage rather than the host FS.
            // Width-guarded (`p57_fs_is_synthetic_shaped`): a genuinely
            // concrete default FileSystem receiver is narrower than
            // `P57_FS_SLOTS` and is never a jar/jrt mount, so it never reads
            // its own real fields here (see `p57_fs_is_virtual`'s doc).
            let (jar, jrt) = if p57_fs_is_synthetic_shaped(ctx, this) {
                let jar = match ctx.get_field(this, P57_FS_JAR_FIELD) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                let jrt = match ctx.get_field(this, P57_FS_JRT_FIELD) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                (jar, jrt)            } else {
                (String::new(), String::new())            };
            // Round 15 wave 5 (lane jdkonly7): the default file system's
            // `getPath` (what `Path.of` delegates to) refuses what `Paths.get`
            // refuses; a jar / jrt mount keeps its own rules.
            if jar.is_empty() && jrt.is_empty() {
                if let Some((input, reason, index)) = p57_path_of_refusal(&first) {
                    return p57_throw_invalid_path(ctx, &input, &reason, index);
                }
            }
            // gc-common w21-b: the receiver is rooted across the Path's
            // allocation and stored through its CURRENT address. It was stored
            // through its entry address, so a collection inside the allocation
            // left every such Path's `fs` slot naming a vacated FileSystem.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let result = if !jar.is_empty() {
                p57_alloc_path(&mut *scope, &jarfs_encode(&jar, &first))
            } else if !jrt.is_empty() {
                p57_alloc_path(&mut *scope, &jrtfs_encode(&jrt, &first))
            } else {
                p57_alloc_path(&mut *scope, &first)
            }?;
            let this = scope.get(&this_h);
            // Record the FileSystem that produced this Path so
            // `Path.getFileSystem()` returns the *same* object — required by
            // identity checks like cassandra's `File(Path)` constructor
            // (`path.getFileSystem() == FileSystems.getDefault()`).
            scope.set_field(result, p57_path_slots_for(scope.vm_identity()).fs, Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        fs_class,
        "provider",
        "()Ljava/nio/file/spi/FileSystemProvider;",
        |ctx, args| {
            // One provider object per FileSystem, cached in
            // P57_FS_PROVIDER_FIELD. This used to allocate a FRESH provider on
            // every call, which made even `fs.provider() == fs.provider()`
            // false; HotSpot answers true (measured, `provider==provider|true`
            // in scratchpad/c14/FsIdentity.java). `p57_fs_provider` still
            // reports the scheme matching this FileSystem, so callers that
            // check `fs.provider().getScheme()` keep seeing "jrt"/"jar" for a
            // mounted virtual FS and "file" otherwise.
            //
            // A receiver we cannot read (no arg 0) has no field to cache in;
            // fall back to the default provider rather than inventing one, so
            // the answer is still the singleton.
            let provider = match obj_arg(args, 0) {
                Ok(this) => p57_fs_provider(ctx, this)?,
                Err(_) => p57_default_provider_singleton(ctx)?,
            };
            Ok(Some(Value::Object(Some(provider))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `defaultDirectory`/`defaultRoot` — package-private, non-native
    // accessors real `sun.nio.fs.WindowsFileSystem` bytecode declares
    // (bare `getfield`/`areturn` over the fields `filesystem_layout`
    // resolves for the CONCRETE default FileSystem). `p57_alloc_jar_
    // filesystem`/`p57_alloc_jrt_filesystem` mint their FileSystem under
    // the ABSTRACT `java/nio/file/FileSystem` name (this crate's
    // width-guarded synthetic shape, `P57_FS_SLOTS`), which has no such
    // method table at all — so real `com.sun.tools.javac.comp.Modules
    // .setCompilationUnitModules` (module-path scanning during in-process
    // compilation) throws `NoSuchMethodError` the moment it reaches a
    // mounted jar/jrt FileSystem, immediately after the jar-mounting fix
    // in `newFileSystem`'s own doc comment (`retired_shadow.rs`) gets past
    // the provider-selection step. A synthetic-shaped mount has no real OS
    // default directory/root, so these return the harmless placeholder
    // `p57_alloc_jar_filesystem` already uses for its separator; a
    // genuinely concrete receiver (the real default FileSystem) reads its
    // own resolved field instead, matching what its real bytecode would
    // have returned had dispatch found it.
    r.register(
        fs_class,
        "defaultDirectory",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if !p57_fs_is_synthetic_shaped(ctx, this) {
                if let Some(slots) = cratonvm_native_api::filesystem_layout::resolve(ctx) {
                    if let Value::Object(Some(s)) = ctx.get_field(this, slots.default_directory) {
                        return Ok(Some(Value::Object(Some(s))));
                    }
                }
            }
            let sep = if cfg!(windows) { "C:\\" } else { "/" };
            let s = ctx.create_string(sep);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fs_class,
        "defaultRoot",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if !p57_fs_is_synthetic_shaped(ctx, this) {
                if let Some(slots) = cratonvm_native_api::filesystem_layout::resolve(ctx) {
                    if let Value::Object(Some(s)) = ctx.get_field(this, slots.default_root) {
                        return Ok(Some(Value::Object(Some(s))));
                    }
                }
            }
            let sep = if cfg!(windows) { "C:\\" } else { "/" };
            let s = ctx.create_string(sep);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // KEEP, with the real-JDK anchor rather than "no data": the concrete
    // default-FileSystem implementations both spell this out as a constant —
    // `sun.nio.fs.UnixFileSystem.isOpen()` and
    // `sun.nio.fs.WindowsFileSystem.isOpen()` are `public final boolean
    // isOpen() { return true; }`. That is the object this native serves in the
    // overwhelming majority of cases.
    //
    // The jar/jrt FileSystems this VM mounts are the one place a real JDK
    // WOULD track state (`ZipFileSystem.isOpen()` reads its `isOpen` flag) —
    // but the paired `close()` registration immediately below is a deliberate
    // no-op for them precisely because they hold no OS handle to release, so
    // no reachable object can ever transition to closed. Making this read a
    // flag would need a 4th slot on the synthetic FileSystem written by
    // `close()`; that is the change to make if/when jar-FS mounts start owning
    // a real handle.
    r.register(fs_class, "isOpen", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1))) // default FS is always open
    }, cratonvm_native_api::NativeKind::Bridge);

    // Was unconditionally writable. A mounted runtime image (`jrt:`) is
    // read-only by definition, and this VM's jar-FS mount is read-only too
    // (`jarfs_*` has no in-place update path), so a caller that checked
    // `isReadOnly()` before attempting a write was told to go ahead and then
    // failed further downstream with an unrelated error.
    r.register(fs_class, "isReadOnly", "()Z", |ctx, args| {
        let virtual_fs = matches!(obj_arg(args, 0), Ok(this) if p57_fs_is_virtual(ctx, this));
        Ok(Some(Value::Int(i32::from(virtual_fs))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // Round 63 — Keycloak 26.6.1 calls FileSystem.close() during shutdown/cleanup
    // paths. FileSystem.close() is abstract in the real JDK; without a native
    // override the synthetic default-FS object throws AbstractMethodError.
    //
    // It used to be an unconditional no-op, which quietly disagreed with the
    // spec: `FileSystem.close()` on the DEFAULT file system throws
    // `UnsupportedOperationException` ("The default file system cannot be
    // closed"), and a caller that closes what it believes is a mounted
    // zip/jar FileSystem — but actually holds the default one — was silently
    // told it succeeded. Throw for the default FS (what HotSpot does, so any
    // caller reaching here already has to handle it) and stay a no-op for a
    // mounted jar/jrt FileSystem, which holds no OS handle to release.
    r.register(fs_class, "close", "()V", |ctx, args| {
        let virtual_fs = matches!(obj_arg(args, 0), Ok(this) if p57_fs_is_virtual(ctx, this));
        if virtual_fs {
            return Ok(None);
        }
        Err(RuntimeError::UnsupportedOperationException {
            message: "The default file system cannot be closed".into(),
        }
        .into())
    }, cratonvm_native_api::NativeKind::Bridge);

    // Supplementary FileSystem methods that are abstract in real JDK and may
    // be invoked on the synthetic default-FS object.
    //
    // Was an unconditional `EmptySet` — real HotSpot's default `FileSystem`
    // never reports zero supported views (that's a Windows-only default-FS
    // possibility that doesn't apply here since we're always backed by a
    // real host filesystem). An empty set made `Files.setPosixFilePermissions`
    // → `getFileAttributeView(path, PosixFileAttributeView.class)` a
    // pointless lookup for callers that pre-check via
    // `fs.supportedFileAttributeViews().contains("posix")`, and is the
    // upstream symptom of the `Files.setPosixFilePermissions`
    // `UnsupportedOperationException` (H2 `FilePathDisk.setReadOnly`/
    // `TestFileSystem.testSetReadOnly`/`TestTraceSystem.testReadOnly`).
    // Report the same view-name set HotSpot reports on each platform. A
    // mounted-jar/runtime-image (virtual) FileSystem has no POSIX-attribute
    // filesystem backing it, so it keeps reporting none.
    r.register(
        fs_class,
        "supportedFileAttributeViews",
        "()Ljava/util/Set;",
        |ctx, args| {
            let is_virtual = matches!(obj_arg(args, 0), Ok(this) if p57_fs_is_virtual(ctx, this));
            if is_virtual {
                let s = try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptySet", 0)?;
                return Ok(Some(Value::Object(Some(s))));
            }
            let names: &[&str] = if cfg!(windows) {
                &["basic", "dos", "acl", "owner", "user"]
            } else {
                // Matches HotSpot on Linux exactly (confirmed via
                // `p.getFileSystem().supportedFileAttributeViews()`):
                // `[owner, dos, basic, posix, user, unix]` — `dos` is
                // included even on Linux (emulated via xattrs since JDK 15).
                &["owner", "dos", "basic", "posix", "user", "unix"]
            };
            // `build_string_set` (naive synthetic array/size/capacity layout)
            // silently prints/iterates as empty under real-JDK mode — real
            // `AbstractCollection.toString()`/`HashSet.iterator()` bytecode
            // reads the REAL `HashSet.map` field expecting a real `HashMap`,
            // not our raw backing array. Use the real-field-layout builder
            // (already established for exactly this class of bug — see its
            // doc comment) instead.
            // gc-common w18-f: each name is rooted across the next one's
            // allocation and handed over at its current address (they were
            // collected raw). `build_real_layout_string_hashset` roots them
            // again across its own class initialisation and allocations
            // (`applied/handoff-w18f-string-hashset-builder-roots-its-keys`).
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let handles: Vec<_> = names
                .iter()
                .map(|s| {
                    let k = scope.create_string(s);
                    scope.root(k)
                })
                .collect();
            let keys: Vec<ObjectRef> = handles.iter().map(|h| scope.get(h)).collect();
            let s = crate::build_real_layout_string_hashset(&mut *scope, &keys)?;
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fs_class,
        "getFileStores",
        "()Ljava/lang/Iterable;",
        |ctx, _args| {
            use cratonvm_types::ArrayElementType;
            let arr = ctx.new_array(ArrayElementType::Reference, 0);
            // Pin across the list alloc below — a moving young GC there would
            // relocate the fresh array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            // By NAME, not by raw slot. These three writes were
            // `set_field(list, 0/1/2, ..)` against the REAL JDK layout
            // (`AbstractList.modCount`, `ArrayList.elementData`,
            // `ArrayList.size`) -- an absolute extent of 3 on a class this
            // code does not own, which is what the T9C gate scores against the
            // synthetic table's 2 and what that gate's own "rule when this
            // fails" says to replace. Resolving by name is layout-independent:
            // correct here, and in synthetic mode a write that finds no such
            // field is dropped rather than landing on the wrong slot.
            ctx.set_field_by_name(list, "modCount", Value::Int(0));
            ctx.set_field_by_name(list, "elementData", Value::Object(Some(arr)));
            ctx.set_field_by_name(list, "size", Value::Int(0));
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(list))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `FileSystem.newWatchService()` deliberately has NO registration here.
    // It used to hand out a bare 1-field placeholder with no platform watcher
    // behind it; `cratonvm-native-io`'s `native_ws_new` is the real
    // implementation (a `notify::RecommendedWatcher` plus the 3-field layout
    // the rest of the WatchService natives read). Registration is
    // last-write-wins, so this placeholder silently displaced it and every
    // later native then operated on an object with none of the expected
    // slots — `Path.register` reported "service is closed or unknown" and
    // `WatchService.close` wrote past the receiver's layout. See
    // `filewatcher-watchservice-surface-FIXED-20260801.md`.

    r.register(
        fs_class,
        "getRootDirectories",
        "()Ljava/lang/Iterable;",
        |ctx, args| {
            use cratonvm_types::ArrayElementType;
            // A mounted-jar FileSystem (P57_FS_JAR_FIELD set) must enumerate from
            // a jar-FS root path so `Files.walkFileTree` recurses into the
            // archive's entries — javac's `JavacFileManager$ArchiveContainer`
            // walks `getRootDirectories()` to index a classpath jar's packages
            // (the in-process compiler the H2 `CREATE ALIAS` / HIB-CV-27 path
            // depends on). A host FileSystem returns the OS root.
            //
            // The previous body ignored `this` and always returned the host
            // root, wrapped in a `Collections$SingletonList` whose field 0 held
            // the backing *array* — but the real-JDK `SingletonList` iterator
            // reads field 0 as the single *element*, so a for-each yielded one
            // null. That null `Path` reached the default `SimpleFileVisitor.
            // visitFile`, whose `Objects.requireNonNull(file)` then NPE'd —
            // crashing `ArchiveContainer.<init>` for every classpath jar and
            // making in-process compilation fail. Use the real-JDK `ArrayList`
            // layout instead (same as `installedProviders`), which iterates
            // correctly.
            // Pin across the path/array/list allocs below — a moving young GC
            // there would relocate `this` and the fresh objects (native
            // stale-local family).
            let this_pin = obj_arg(args, 0).ok().map(|t| (ctx.pin_native_root(t), t));
            // Width-guarded (`p57_fs_is_synthetic_shaped`) for the same
            // reason `getPath` above is: a genuinely concrete default
            // FileSystem receiver is never a jar/jrt mount and must not have
            // its own real fields read as if they were.
            let jar_field = this_pin.and_then(|(_, this)| {
                p57_fs_is_synthetic_shaped(ctx, this).then(|| ctx.get_field(this, P57_FS_JAR_FIELD))
            });
            let jrt_field = this_pin.and_then(|(_, this)| {
                p57_fs_is_synthetic_shaped(ctx, this).then(|| ctx.get_field(this, P57_FS_JRT_FIELD))
            });
            let root_path = match (jar_field, jrt_field) {
                (Some(Value::Object(Some(s))), _) => {
                    let jar = ctx.read_string(s).unwrap_or_default();
                    let rp = p57_alloc_path(ctx, &jarfs_encode(&jar, ""))?;
                    if let Some((h, orig)) = this_pin {
                        let this = ctx.read_native_pin(h, orig);
                        ctx.set_field(rp, p57_path_slots_for(ctx.vm_identity()).fs, Value::Object(Some(this)));
                    }
                    rp
                }
                (_, Some(Value::Object(Some(s)))) => {
                    // Runtime image: root is the synthetic `/` whose only child is
                    // `/modules` (javac walks `/modules/<module>/...`).
                    let jh = ctx.read_string(s).unwrap_or_default();
                    let rp = p57_alloc_path(ctx, &jrtfs_encode(&jh, ""))?;
                    if let Some((h, orig)) = this_pin {
                        let this = ctx.read_native_pin(h, orig);
                        ctx.set_field(rp, p57_path_slots_for(ctx.vm_identity()).fs, Value::Object(Some(this)));
                    }
                    rp
                }
                _ => {
                    let root = if cfg!(windows) { "C:\\" } else { "/" };
                    p57_alloc_path(ctx, root)?
                }
            };
            let root_path_pin = ctx.pin_native_root(root_path);
            let arr = ctx.new_array(ArrayElementType::Reference, 1);
            let arr_pin = ctx.pin_native_root(arr);
            let root_path = ctx.read_native_pin(root_path_pin, root_path);
            ctx.set_array_element(arr, 0, Value::Object(Some(root_path)));
            // Real ArrayList field layout in real-JDK mode:
            //   [0]=AbstractList.modCount (int), [1]=elementData (Object[]), [2]=size (int).
            // gc-common w15-f: typed so the stale-handle audit sees the binding;
            // this allocation is the last GC point before the stores below.
            let list: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            // By NAME, not by raw slot. These three writes were
            // `set_field(list, 0/1/2, ..)` against the REAL JDK layout
            // (`AbstractList.modCount`, `ArrayList.elementData`,
            // `ArrayList.size`) -- an absolute extent of 3 on a class this
            // code does not own, which is what the T9C gate scores against the
            // synthetic table's 2 and what that gate's own "rule when this
            // fails" says to replace. Resolving by name is layout-independent:
            // correct here, and in synthetic mode a write that finds no such
            // field is dropped rather than landing on the wrong slot.
            ctx.set_field_by_name(list, "modCount", Value::Int(0));
            ctx.set_field_by_name(list, "elementData", Value::Object(Some(arr)));
            ctx.set_field_by_name(list, "size", Value::Int(1));
            ctx.unpin_native_roots(this_pin.map(|(h, _)| h).unwrap_or(root_path_pin));
            Ok(Some(Value::Object(Some(list))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Files.probeContentType, the Windows half ---
    //
    // `Files.probeContentType` walks the installed `FileTypeDetector`s and then
    // the platform default, which on Windows is `sun.nio.fs.RegistryFileTypeDetector`
    // — a `HKEY_CLASSES_ROOT\<ext>` "Content Type" lookup. Its
    // `implProbeContentType` is pure JDK bytecode except for two things, and
    // BOTH of them were missing:
    //
    //     at sun.nio.fs.WindowsNativeDispatcher.<clinit>(WindowsNativeDispatcher.java:1100)
    //     at sun.nio.fs.RegistryFileTypeDetector.implProbeContentType(…:55)
    //     at sun.nio.fs.AbstractFileTypeDetector.probeContentType(…:75)
    //     at java.nio.file.Files.probeContentType(Files.java:1594)
    //
    // Registering these two rather than shadowing `implProbeContentType` is the
    // point: the JDK's own detector body then runs, and with it
    // `AbstractFileTypeDetector.probeContentType`'s fallback to
    // `URLConnection.getFileNameMap()` and its `parse()` validation — which is
    // where `.js` comes from (`text/javascript`), because the registry has no
    // entry for it. Shadowing the detector would have needed a `check_override`
    // entry, i.e. exactly the "prefer our native over real JDK bytecode"
    // exception that chain's own banner says must stop growing.
    //
    // Measured against HotSpot on the same machine, all 13 lines of
    // `probes/ProbeContentType.java`: registry for `.txt`/`.html`/`.png`/
    // `.json`/`.pdf`/`.xml`/`.zip`/`.css`, JDK fallback for `.js`, `null` for
    // an unknown extension, a name with no dot, and a directory.
    #[cfg(windows)]
    {
        // `initIDs` caches jfieldIDs for the JNI layer. There is no JNI layer
        // here and nothing to cache, so a no-op is the honest implementation,
        // not a stub.
        //
        // Consequence worth stating: `WindowsNativeDispatcher`'s class
        // initialiser now SUCCEEDS, so its ~80 other natives stop being
        // unreachable behind a failing `<clinit>` and start failing
        // individually. That is strictly more informative — the same
        // `UnsatisfiedLinkError`, naming the native the caller actually
        // wanted — but it does move where the error appears.
        r.register(
            "sun/nio/fs/WindowsNativeDispatcher",
            "initIDs",
            "()V",
            |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
        );

        // `private static native void GetFileAttributesEx0(long, long)
        //     throws WindowsException` — `javap -c` on
        // `WindowsFileAttributes.get`: both arguments are addresses of
        // `NativeBuffer`s real bytecode already allocated via
        // `Unsafe.allocateMemory` (a TAGGED arena handle in this VM, not a
        // raw OS pointer — confirmed via `NativeBuffer`'s own `<init>`
        // bytecode), the first holding the NUL-terminated UTF-16 path, the
        // second a 36-byte buffer this call fills with the raw
        // `WIN32_FILE_ATTRIBUTE_DATA` struct real
        // `WindowsFileAttributes.fromFileAttributeData` decodes by fixed
        // offset. See `win32_get_file_attributes_ex`'s own doc comment for
        // the byte layout and why it needs no repacking.
        r.register(
            "sun/nio/fs/WindowsNativeDispatcher",
            "GetFileAttributesEx0",
            "(JJ)V",
            |ctx, args| {
                let path_addr = match args.first() {
                    Some(Value::Long(v)) => *v,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("path buffer address is not a long".into()),
                        }
                        .into())
                    }
                };
                let out_addr = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("output buffer address is not a long".into()),
                        }
                        .into())
                    }
                };
                let path = match read_native_utf16_c_string(ctx, path_addr) {
                    Some(p) => p,
                    None => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "GetFileAttributesEx0: unreadable path buffer".into(),
                        }
                        .into())
                    }
                };
                let mut wide: Vec<u16> = path.encode_utf16().collect();
                wide.push(0);
                match win32_get_file_attributes_ex(&wide) {
                    Ok(bytes) => {
                        if !ctx.copy_to_native_memory(out_addr, &bytes) {
                            return Err(RuntimeError::IllegalStateException {
                                message: "GetFileAttributesEx0: output buffer write failed".into(),
                            }
                            .into());
                        }
                        Ok(None)
                    }
                    Err(win32_error) => Err(cratonvm_native_api::throwable_builder::throw_new(
                        ctx,
                        "sun/nio/fs/WindowsException",
                        "(I)V",
                        &[Value::Int(win32_error as i32)],
                        || {
                            RuntimeError::IOException {
                                message: format!(
                                    "GetFileAttributesEx0 failed, Win32 error {win32_error}"
                                ),
                            }
                            .into()
                        },
                    )),
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );

        // `private static native long CreateFile0(long, int, int, long, int,
        //     int) throws WindowsException` — `javap -c` on
        // `WindowsNativeDispatcher.CreateFile(String,int,int,long,int,int)`:
        // arg 0 is a `NativeBuffer` address holding the NUL-terminated
        // UTF-16 path (same convention as `GetFileAttributesEx0` above),
        // args 1/2/4/5 are `dwDesiredAccess`/`dwShareMode`/
        // `dwCreationDisposition`/`dwFlagsAndAttributes` passed straight
        // through, and arg 3 is `lpSecurityAttributes` — always `0` at
        // every real call site. See `win32_create_file`'s own doc comment
        // for why a non-zero value is refused rather than guessed at.
        r.register(
            "sun/nio/fs/WindowsNativeDispatcher",
            "CreateFile0",
            "(JIIJII)J",
            |ctx, args| {
                let path_addr = match args.first() {
                    Some(Value::Long(v)) => *v,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("path buffer address is not a long".into()),
                        }
                        .into())
                    }
                };
                let desired_access = match args.get(1) {
                    Some(Value::Int(v)) => *v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "CreateFile0: dwDesiredAccess is not an int".into(),
                        }
                        .into())
                    }
                };
                let share_mode = match args.get(2) {
                    Some(Value::Int(v)) => *v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "CreateFile0: dwShareMode is not an int".into(),
                        }
                        .into())
                    }
                };
                let security_attributes = match args.get(3) {
                    Some(Value::Long(v)) => *v,
                    _ => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "CreateFile0: lpSecurityAttributes is not a long".into(),
                        }
                        .into())
                    }
                };
                let creation_disposition = match args.get(4) {
                    Some(Value::Int(v)) => *v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "CreateFile0: dwCreationDisposition is not an int".into(),
                        }
                        .into())
                    }
                };
                let flags_and_attributes = match args.get(5) {
                    Some(Value::Int(v)) => *v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "CreateFile0: dwFlagsAndAttributes is not an int".into(),
                        }
                        .into())
                    }
                };
                // The handle goes into the fd table as a `std::fs::File`, whose reads and
                // writes are synchronous: `std` aborts the process ("operation failed to
                // complete synchronously") on an overlapped handle. The IOCP machinery that
                // `FILE_FLAG_OVERLAPPED` serves (an `AsynchronousFileChannel` opened with an
                // executor, as Vert.x does) is not backed by this VM, so open synchronously.
                const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
                let flags_and_attributes = flags_and_attributes & !FILE_FLAG_OVERLAPPED;
                let path = match read_native_utf16_c_string(ctx, path_addr) {
                    Some(p) => p,
                    None => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "CreateFile0: unreadable path buffer".into(),
                        }
                        .into())
                    }
                };
                let mut wide: Vec<u16> = path.encode_utf16().collect();
                wide.push(0);
                match win32_create_file(
                    &wide,
                    desired_access,
                    share_mode,
                    security_attributes,
                    creation_disposition,
                    flags_and_attributes,
                ) {
                    Ok(handle) => {
                        // Absorb the raw Win32 HANDLE into this VM's own
                        // fd_table id space immediately — see
                        // `FdTable::insert_win32_file`'s doc comment for why
                        // handing the raw value to Java would be unsafe
                        // (every existing native that reads
                        // `FileDescriptor.fd`/`.handle` already treats it as
                        // an fd_table id, and a real kernel HANDLE can alias
                        // one).
                        //
                        // SAFETY: `handle` is a HANDLE this call just
                        // received from `CreateFileW` and has not shared
                        // with anything else; `File::from_raw_handle` takes
                        // ownership of it, so it is closed exactly once,
                        // when the fd_table entry is dropped.
                        use std::os::windows::io::FromRawHandle;
                        let file = unsafe {
                            std::fs::File::from_raw_handle(handle as *mut std::ffi::c_void)
                        };
                        let fd_id = ctx.fd_table().insert_win32_file(file);
                        Ok(Some(Value::Long(fd_id as i64)))
                    }
                    Err(win32_err) => {
                        let win32_error = match win32_err {
                            WindowsCreateFileError::Win32(code) => code,
                            WindowsCreateFileError::UnsupportedSecurityAttributes => {
                                return Err(RuntimeError::IllegalStateException {
                                    message:
                                        "CreateFile0: non-null lpSecurityAttributes is not supported"
                                            .into(),
                                }
                                .into())
                            }
                        };
                        Err(cratonvm_native_api::throwable_builder::throw_new(
                            ctx,
                            "sun/nio/fs/WindowsException",
                            "(I)V",
                            &[Value::Int(win32_error as i32)],
                            || {
                                RuntimeError::IOException {
                                    message: format!(
                                        "CreateFile0 failed, Win32 error {win32_error}"
                                    ),
                                }
                                .into()
                            },
                        ))
                    }
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );

        // `private static native String GetFullPathName0(long)
        //     throws WindowsException` — `javap -c` on
        // `WindowsNativeDispatcher.GetFullPathName(String)`: the sole
        // argument is a `NativeBuffer` address holding the NUL-terminated
        // UTF-16 path (same convention as `GetFileAttributesEx0`/
        // `CreateFile0` above); the wrapper returns whatever this native
        // returns with no further post-processing, so this call mirrors raw
        // `GetFullPathNameW` output verbatim rather than reusing
        // `win_get_full_path_name`'s JDK-`canonicalize`-flavoured trimming.
        r.register(
            "sun/nio/fs/WindowsNativeDispatcher",
            "GetFullPathName0",
            "(J)Ljava/lang/String;",
            |ctx, args| {
                let path_addr = match args.first() {
                    Some(Value::Long(v)) => *v,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("path buffer address is not a long".into()),
                        }
                        .into())
                    }
                };
                let path = match read_native_utf16_c_string(ctx, path_addr) {
                    Some(p) => p,
                    None => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "GetFullPathName0: unreadable path buffer".into(),
                        }
                        .into())
                    }
                };
                let mut wide: Vec<u16> = path.encode_utf16().collect();
                wide.push(0);
                match win32_get_full_path_name0(&wide) {
                    Ok(full) => {
                        let s = ctx.create_string(&full);
                        Ok(Some(Value::Object(Some(s))))
                    }
                    Err(win32_error) => Err(cratonvm_native_api::throwable_builder::throw_new(
                        ctx,
                        "sun/nio/fs/WindowsException",
                        "(I)V",
                        &[Value::Int(win32_error as i32)],
                        || {
                            RuntimeError::IOException {
                                message: format!(
                                    "GetFullPathName0 failed, Win32 error {win32_error}"
                                ),
                            }
                            .into()
                        },
                    )),
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );

        // `private static int GetFileAttributes0(long)
        //     throws WindowsException` — `javap -c` on
        // `WindowsNativeDispatcher.GetFileAttributes(String)`: a
        // `NativeBuffer` address holding the NUL-terminated UTF-16 path
        // (same convention as the three natives above), returning the raw
        // `DWORD` attributes bitmask directly rather than through an output
        // buffer -- unlike `GetFileAttributesEx0`, which decodes a full
        // `WIN32_FILE_ATTRIBUTE_DATA` for `WindowsFileAttributes`. Reached
        // by `WindowsLinkSupport.getRealPath`'s final-segment existence
        // check (real path has no further `\` after the walked prefix).
        r.register(
            "sun/nio/fs/WindowsNativeDispatcher",
            "GetFileAttributes0",
            "(J)I",
            |ctx, args| {
                let path_addr = match args.first() {
                    Some(Value::Long(v)) => *v,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("path buffer address is not a long".into()),
                        }
                        .into())
                    }
                };
                let path = match read_native_utf16_c_string(ctx, path_addr) {
                    Some(p) => p,
                    None => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "GetFileAttributes0: unreadable path buffer".into(),
                        }
                        .into())
                    }
                };
                let mut wide: Vec<u16> = path.encode_utf16().collect();
                wide.push(0);
                match win32_get_file_attributes0(&wide) {
                    Ok(attrs) => Ok(Some(Value::Int(attrs as i32))),
                    Err(win32_error) => Err(cratonvm_native_api::throwable_builder::throw_new(
                        ctx,
                        "sun/nio/fs/WindowsException",
                        "(I)V",
                        &[Value::Int(win32_error as i32)],
                        || {
                            RuntimeError::IOException {
                                message: format!(
                                    "GetFileAttributes0 failed, Win32 error {win32_error}"
                                ),
                            }
                            .into()
                        },
                    )),
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );

        // `private static native void FindFirstFile0(long,
        //     WindowsNativeDispatcher.FirstFile) throws WindowsException` —
        // `javap -c` on `WindowsNativeDispatcher.FindFirstFile(String)`: a
        // `NativeBuffer` address for the path (same convention as above)
        // and a pre-constructed `FirstFile` instance (`javap -p`: private
        // `long handle`, `String name`, `int attributes`, no public
        // setters) that this native fills BY NAME via `set_field_by_name`,
        // the same mechanism every other Java-object-out-param native in
        // this VM uses. Reached by `WindowsLinkSupport.getRealPath`'s
        // per-component case-correction walk -- the loop this VM's OWN
        // `win_case_correct` (this file, `FindFirstFileW`-based) already
        // performs for a different caller, but real `WindowsPath.
        // toRealPath()` dispatches through this native, not through that
        // Rust helper, so the two do not share a call site.
        r.register(
            "sun/nio/fs/WindowsNativeDispatcher",
            "FindFirstFile0",
            "(JLsun/nio/fs/WindowsNativeDispatcher$FirstFile;)V",
            |ctx, args| {
                let path_addr = match args.first() {
                    Some(Value::Long(v)) => *v,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("path buffer address is not a long".into()),
                        }
                        .into())
                    }
                };
                let out_ref = match args.get(1) {
                    Some(Value::Object(Some(r))) => *r,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("FindFirstFile0: null FirstFile receiver".into()),
                        }
                        .into())
                    }
                };
                let path = match read_native_utf16_c_string(ctx, path_addr) {
                    Some(p) => p,
                    None => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "FindFirstFile0: unreadable path buffer".into(),
                        }
                        .into())
                    }
                };
                let mut wide: Vec<u16> = path.encode_utf16().collect();
                wide.push(0);
                match win32_find_first_file(&wide) {
                    Ok((handle, attributes, name, _raw)) => {
                        // `create_string` is a GC point: pin the out-param.
                        let out_pin = ctx.pin_native_root(out_ref);
                        let name_str = ctx.create_string(&name);
                        let out_ref = ctx.read_native_pin(out_pin, out_ref);
                        ctx.set_field_by_name(out_ref, "handle", Value::Long(handle));
                        ctx.set_field_by_name(out_ref, "name", Value::Object(Some(name_str)));
                        ctx.set_field_by_name(out_ref, "attributes", Value::Int(attributes as i32));
                        ctx.unpin_native_roots(out_pin);
                        Ok(None)
                    }
                    Err(win32_error) => Err(cratonvm_native_api::throwable_builder::throw_new(
                        ctx,
                        "sun/nio/fs/WindowsException",
                        "(I)V",
                        &[Value::Int(win32_error as i32)],
                        || {
                            RuntimeError::IOException {
                                message: format!(
                                    "FindFirstFile0 failed, Win32 error {win32_error}"
                                ),
                            }
                            .into()
                        },
                    )),
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );

        // `static native void FindClose(long) throws WindowsException` --
        // no `FindClose0` wrapper in real bytecode (`javap`: this one IS
        // the native, package-visible, no `asNativeBuffer` indirection
        // since a search handle is not a path). Closes the handle
        // `FindFirstFile0` opened once `WindowsLinkSupport.getRealPath`'s
        // per-component walk is done reading it.
        r.register(
            "sun/nio/fs/WindowsNativeDispatcher",
            "FindClose",
            "(J)V",
            |ctx, args| {
                let handle = match args.first() {
                    Some(Value::Long(v)) => *v,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("search handle is not a long".into()),
                        }
                        .into())
                    }
                };
                match win32_find_close(handle) {
                    Ok(()) => Ok(None),
                    Err(win32_error) => Err(cratonvm_native_api::throwable_builder::throw_new(
                        ctx,
                        "sun/nio/fs/WindowsException",
                        "(I)V",
                        &[Value::Int(win32_error as i32)],
                        || {
                            RuntimeError::IOException {
                                message: format!("FindClose failed, Win32 error {win32_error}"),
                            }
                            .into()
                        },
                    )),
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );

        // Wave 15: the path-only link/copy/move/delete/directory/attribute
        // natives (`CreateSymbolicLink0`, `CreateHardLink0`, `DeleteFile0`,
        // `RemoveDirectory0`, `CreateDirectory0`, `MoveFileEx0`, `CopyFileEx0`,
        // `SetFileAttributes0`).
        win32_register_link_natives(r);
        // Wave 16: directory search continuation, volume/drive queries and the
        // handle-taking natives (`SetFileTime0`, `SetEndOfFile`,
        // `DeviceIoControl*`, `GetFinalPathNameByHandle`).
        win32_register_fs_natives(r);
        // Security descriptors: the owner lookup `Files.getOwner` makes.
        win32_register_security_natives(r);
        // Process/thread tokens and privileges: the `WindowsSecurity` class
        // initialiser and `enablePrivilege`, which `Files.copy(..,
        // COPY_ATTRIBUTES)` reaches through `WindowsFileCopy`.
        win32_register_token_natives(r);

        // `private static native String queryStringValue(long subKey, long name)`.
        // Both arguments are addresses of NUL-terminated UTF-16 strings in
        // `NativeBuffer`s that `WindowsNativeDispatcher.asNativeBuffer` filled
        // through `Unsafe` — so they may be arena handles or real pointers, and
        // `ctx.copy_from_native_memory` is the bridge that already knows which.
        //
        // The JNI original returns NULL for a missing key or value and does not
        // throw; `AbstractFileTypeDetector` reads that null as "no answer" and
        // falls back. Do the same.
        r.register(
            "sun/nio/fs/RegistryFileTypeDetector",
            "queryStringValue",
            "(JJ)Ljava/lang/String;",
            |ctx, args| {
                let sub_key = match args.first() {
                    Some(Value::Long(v)) => *v,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let value_name = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let (Some(key), Some(name)) = (
                    read_native_utf16_c_string(ctx, sub_key),
                    read_native_utf16_c_string(ctx, value_name),
                ) else {
                    return Ok(Some(Value::Object(None)));
                };
                match hkcr_string_value(&key, &name) {
                    Some(v) => {
                        let s = ctx.create_string(&v);
                        Ok(Some(Value::Object(Some(s))))
                    }
                    None => Ok(Some(Value::Object(None))),
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // --- FileSystemProvider minimal methods ---
    // Captured before ANY fsp registration below, same since-before-the-block
    // shape `__path_rows_before` uses, so the mirror after the last fsp
    // registration (`newFileChannel`) sees the whole family.
    let __fsp_rows_before = r.dump_registrations().len();
    let fsp = "java/nio/file/spi/FileSystemProvider";
    r.register(fsp, "getScheme", "()Ljava/lang/String;", |ctx, args| {
        retired_receiver_yield!(ctx, args, "getScheme", "()Ljava/lang/String;");
        // The scheme is a PRIVATE slot above the concrete provider class's own
        // layout — `file_system_provider_base` is the same accessor
        // `p57_alloc_provider` writes through, and it collapses to 0 for any
        // receiver this crate did not allocate (a stub-mode object, or a real
        // `sun.nio.fs.*Provider` built by JDK bytecode), which is exactly the
        // legacy behaviour.
        //
        // Falls back to "file" when the slot holds no String — the answer for
        // the default provider, and the only provider a legacy caller that
        // allocated without a scheme slot could have meant.
        if let Ok(this) = obj_arg(args, 0) {
            let base = file_system_provider_base(ctx, this);
            if let Value::Object(Some(s)) = ctx.get_field(this, base) {
                if ctx.read_string(s).is_some() {
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
        }
        let s = ctx.create_string("file");
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // FileSystemProvider.isSameFile(Path, Path) — abstract on the base, so the
    // synthetic provider lacking it made `Files.isSameFile` dispatch to the
    // abstract method ("no Code attribute" AbstractMethodError).
    //
    // The comment that used to sit here said the default provider's same-file
    // check *is* path equality. It is not: equality is the provider's FAST PATH.
    // Both default providers return early on `file1.equals(obj2)` and otherwise
    // read the IDENTITY of both files — `st_dev`/`st_ino` on Unix, volume serial
    // plus file index via `GetFileInformationByHandle` on Windows — and compare
    // that. So the JDK answers `true` and this answered `false` for every pair
    // naming one file by two spellings: a hard link, a symlink and its target,
    // `dir/x` versus `dir/sub/../x`, an absolute versus a relative path, and on
    // Windows two spellings differing only in case or in 8.3 shortening.
    //
    // That is not a harmless approximation. `Files.isSameFile` is how a caller
    // asks "am I about to copy this file onto itself", so `false` is the answer
    // that lets the destructive branch run — W7-8's species, not an exception to
    // it. The identity read is `native-io`'s `paths_name_the_same_file`, which
    // carries the JDK's equality fast path itself.
    r.register(
        fsp,
        "isSameFile",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Z",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "isSameFile",
                "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Z"
            );
            let mut p1 = match obj_arg(args, 1) {
                Ok(p) => p,
                Err(_) => return Ok(Some(Value::Int(0))),
            };
            let mut p2 = match args.get(2) {
                Some(Value::Object(Some(p))) => *p,
                _ => return Ok(Some(Value::Int(0))),
            };
            let (s1, s2) = p57_read_path_pair(ctx, &mut p1, &mut p2);
            // The VFS screen is at the CALL SITE and not in the helper, and it
            // has to be: a jar/jrt-encoded path names no host file, so handing
            // it to an identity read would `stat` a path that cannot exist and
            // turn a legitimate `false` into a `NoSuchFileException`. Inside an
            // archive, two entries are the same entry exactly when they spell
            // the same entry — there are no links and no `..` to resolve — so
            // equality is the whole answer there rather than a fast path.
            if vfs_decode(&s1).is_some() || vfs_decode(&s2).is_some() {
                return Ok(Some(Value::Int(i32::from(s1 == s2))));
            }
            match cratonvm_native_io::file_channel::paths_name_the_same_file(
                std::path::Path::new(&s1),
                std::path::Path::new(&s2),
            ) {
                Ok(same) => Ok(Some(Value::Int(i32::from(same)))),
                // `Files.isSameFile` is declared `throws IOException` and the
                // JDK's `NoSuchFileException` is what a caller gets when either
                // path is absent — only reachable for two DIFFERENT spellings,
                // because equal ones never touch the disk.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let missing = if std::path::Path::new(&s1).symlink_metadata().is_err() {
                        s1
                    } else {
                        s2
                    };
                    Err(p57_no_such_file(ctx, &missing)?)
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // FileSystemProvider.installedProviders() — real JDK uses ServiceLoader to
    // discover providers including jdk.nio.zipfs.ZipFileSystemProvider for the
    // "jar" scheme.  Under CratonVM the ServiceLoader path doesn't surface it,
    // so smallrye's ClassPathUtils$JarProviderHolder.<clinit> throws
    // NoSuchElementException("Unable to find provider supporting jar scheme")
    // which kills Quarkus boot silently.  Return a 2-element ArrayList containing
    // synthetic "file" and "jar" providers (scheme stored in instance field 0).
    // FileSystemProvider.newFileSystem(Path, Map) — abstract by default and the
    // base method throws UnsupportedOperationException.  smallrye's
    // ClassPathUtils.processAsJarPath invokes this on the "jar" provider to mount
    // a jar's interior filesystem; we don't model that, so hand back the platform
    // default FileSystem so subsequent getPath("/")/resolve(name)/Files.isDirectory
    // calls return a non-directory non-existent path, the smallrye Function
    // callback yields no config sources, and the loop exits cleanly.
    r.register(
        fsp,
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "newFileSystem",
                "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;"
            );
            // arg 0 = provider (this); arg 1 = Path to the JAR to mount.
            // Mount the JAR's interior as a real jar-backed FileSystem so
            // `getPath`/`resolve`/`Files.*` resolve to archive entries rather
            // than non-existent host paths (smallrye ClassPathUtils relies on
            // walking the mounted jar to find config sources).
            let jar_path = obj_arg(args, 1)
                .ok()
                .map(|mut p| p57_read_path(ctx, &mut p))
                .unwrap_or_default();
            let fs = p57_alloc_jar_filesystem(ctx, &jar_path)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // FileSystemProvider.newFileSystem(URI, Map) — the URI overload of the
    // above. JUnit5's classpath scanner (CloseablePath.create) mounts a JAR
    // via `FileSystems.newFileSystem(URI.create("jar:file:/...!/"), Map.of())`;
    // the real-JDK FileSystems bytecode matches our synthetic "jar" provider
    // (installedProviders below) and invokes this overload on it. Without a
    // native the dispatch lands on the abstract declaration —
    // `AbstractMethodError: newFileSystem(URI, Map) has no Code attribute` —
    // killing all package/classpath-root test discovery. Mount the same
    // jar-backed FileSystem the (Path, Map) overload produces.
    r.register(
        fsp,
        "newFileSystem",
        "(Ljava/net/URI;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "newFileSystem",
                "(Ljava/net/URI;Ljava/util/Map;)Ljava/nio/file/FileSystem;"
            );
            let uri = obj_arg(args, 1)?;
            let text = p57_uri_full_text(ctx, uri);
            // A jrt: URI mounts the runtime image (HIB-CV-27 in-process javac).
            if text.starts_with("jrt:") {
                let jh = ctx.get_system_property("java.home").unwrap_or_default();
                let fs = p57_alloc_jrt_filesystem(ctx, &jh)?;
                return Ok(Some(Value::Object(Some(fs))));
            }
            if let Some((jar, _entry)) = p57_jar_uri_to_entry_path(&text) {
                // Mount file-backed jar/zip URIs even when the archive does not
                // exist yet. HotSpot's zipfs supports Map.of("create", "true");
                // callers then populate it through Files.createDirectories/copy.
                // CratonVM's jarfs writer below creates the archive lazily.
                let fs = p57_alloc_jar_filesystem(ctx, &jar)?;
                return Ok(Some(Value::Object(Some(fs))));
            }
            let fs = p57_alloc_default_filesystem(ctx)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // FileSystemProvider.getFileAttributeView(Path, Class, LinkOption[]) —
    // the synthetic default provider has no native for this, so
    // `Files.getFileAttributeView` dispatch lands on the abstract
    // declaration: "AbstractMethodError ... has no Code attribute" (Gradle
    // ProjectBuilder file writes in Spring Boot buildSrc
    // GenerateAntoraPlaybookTests / DocumentAutoConfigurationClassesTests).
    //
    // The JDK contract: `BasicFileAttributeView` is ALWAYS supported, so a
    // request for it (or the `FileAttributeView` supertype) must return a real
    // view — NOT null. Returning null for the basic view made Gradle's
    // `FileMetadataAccessor`/`Stat` treat freshly-created cache dirs (e.g.
    // `…/userHome/caches/9.5.0`) as "not a directory" → `UncheckedIOException`
    // → the whole `BuildScopeServices` cascade failed (SB-14). We return a
    // synthetic `BasicFileAttributeView` holding the Path; its `readAttributes()`
    // reuses the canonical 5-field BFA builder. Unsupported views (Posix/Dos/…)
    // still return null, matching HotSpot-on-Windows for those types.
    let __view_rows_before = r.dump_registrations().len();
    r.register(
        fsp,
        "getFileAttributeView",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "getFileAttributeView",
                "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;"
            );
            let view_name = obj_arg(args, 2)
                .ok()
                .and_then(|m| crate::lang_class::mirror_class_name(ctx, m))
                .unwrap_or_default();
            // Basic (+ the generic supertype) is always available; Dos is the
            // view Gradle's file metadata uses on Windows (and Dos *extends*
            // Basic). Posix and other views remain null (matching HotSpot on
            // Windows for those types).
            let is_dos = view_name.ends_with("DosFileAttributeView");
            let is_basic = view_name.ends_with("BasicFileAttributeView")
                || view_name.ends_with("/FileAttributeView")
                || view_name == "java/nio/file/attribute/FileAttributeView";
            // Posix is real on Linux/macOS (never on Windows, matching
            // HotSpot-on-Windows null for this type). Was missing entirely,
            // so `Files.setPosixFilePermissions`'s real bytecode (which calls
            // `getFileAttributeView(path, PosixFileAttributeView.class)` and
            // throws `UnsupportedOperationException` on a null result) always
            // threw on Linux too — see
            // docs/feature-designs/native-dispatch-architecture.md.
            let is_posix = !cfg!(windows)
                && (view_name.ends_with("PosixFileAttributeView")
                    // `PosixFileAttributeView extends FileOwnerAttributeView`,
                    // so on a posix host asking for the OWNER view must resolve
                    // to the same object. It resolved to null, and real
                    // `Files.getOwner` is
                    //
                    //   FileOwnerAttributeView view =
                    //       getFileAttributeView(path, FileOwnerAttributeView.class, options);
                    //   if (view == null) throw new UnsupportedOperationException();
                    //
                    // so every `Files.getOwner` answered UOE on Linux once the
                    // real bytecode ran -- including for a path that does not
                    // exist, where the owed answer is `NoSuchFileException`.
                    //
                    //   Files.getOwner(<a real file>)   HotSpot an owner, this VM UOE
                    //   Files.getOwner(<missing>)       HotSpot NoSuchFile,  this VM UOE
                    //
                    // MEASURED with the dial armed over `java/nio/file/Files`.
                    // Same omission the posix view itself had before
                    // `bug-h2-files-setposixfilepermissions-unsupported.md`.
                    || view_name.ends_with("FileOwnerAttributeView"));
            let supported = is_dos || is_basic || is_posix;
            if crate::nbflags().dbg_fsp {
                eprintln!(
                    "[FSP-DBG] getFileAttributeView requested view={view_name} -> {}",
                    if is_dos { "dos-view" } else if is_posix { "posix-view" } else if is_basic { "basic-view" } else { "null" }
                );
            }
            if !supported {
                return Ok(Some(Value::Object(None)));
            }
            let path_obj = match obj_arg(args, 1) {
                Ok(p) => p,
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            let vclass = if is_dos {
                "java/nio/file/attribute/DosFileAttributeView"
            } else if is_posix {
                "java/nio/file/attribute/PosixFileAttributeView"
            } else {
                "java/nio/file/attribute/BasicFileAttributeView"
            };
            // Pin across the view alloc below — a moving young GC there would
            // relocate the Path (native stale-local family).
            let path_pin = ctx.pin_native_root(path_obj);
            let vclass = p57_concrete_view_class(ctx, vclass);
            let view = try_alloc_concurrent_synthetic(ctx, vclass, 1)?;
            let path_obj = ctx.read_native_pin(path_pin, path_obj);
            ctx.set_field(view, 0, Value::Object(Some(path_obj)));
            ctx.unpin_native_roots(path_pin);
            Ok(Some(Value::Object(Some(view))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Basic/Dos FileAttributeView.readAttributes() / name() for the synthetic
    // views returned above. `readAttributes()` reuses the canonical 5-field BFA
    // builder (same path as `Files.readAttributes`) so `isDirectory()` etc. are
    // correct; the Dos variant copies those 5 slots into a `DosFileAttributes`.
    r.register(
        "java/nio/file/attribute/BasicFileAttributeView",
        "readAttributes",
        "()Ljava/nio/file/attribute/BasicFileAttributes;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path_obj = ctx.get_field(this, 0);
            p59_files_read_attributes(ctx, &[path_obj])
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/nio/file/attribute/BasicFileAttributeView",
        "name",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string("basic"))))), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/nio/file/attribute/DosFileAttributeView",
        "readAttributes",
        "()Ljava/nio/file/attribute/DosFileAttributes;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path_obj = ctx.get_field(this, 0);
            let path = extract_path_string(ctx, Some(&path_obj));
            let bfa = p59_files_read_attributes(ctx, &[path_obj])?;
            let bfa_obj = match bfa {
                Some(Value::Object(Some(o))) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // JVMS 6.5 (nio21 close): over a real Windows image the object
            // `p59_files_read_attributes` answers IS the JDK's
            // `sun.nio.fs.WindowsFileAttributes`, which implements
            // `DosFileAttributes` (HotSpot answers exactly that class), and
            // `Files.readAttributes(path, DosFileAttributes.class)` already
            // hands it back as it is. Do the same here instead of minting the
            // INTERFACE-stamped carrier below.
            {
                let cid = ctx.class_id_of_object(bfa_obj);
                if ctx
                    .class_name_arc_of_id(cid)
                    .is_some_and(|n| &*n == "sun/nio/fs/WindowsFileAttributes")
                {
                    return Ok(Some(Value::Object(Some(bfa_obj))));
                }
            }
            // Read through the ACCESSOR helpers rather than copying slots
            // 0..4 verbatim: on a real-JDK boot `basic_file_attributes_alloc`
            // hands back a genuine `sun.nio.fs.{Unix,Windows}FileAttributes`
            // whose slots 0..4 are NOT (creation, access, modified, isDir,
            // size), so the old raw copy filled every Dos slot with garbage
            // — `isDirectory()` on a `DosFileAttributes` was answering from
            // whatever happened to live in the real class's fourth field.
            let creation = basic_file_attributes_time_millis(ctx, bfa_obj, "creation");
            let access = basic_file_attributes_time_millis(ctx, bfa_obj, "access");
            let modified = basic_file_attributes_time_millis(ctx, bfa_obj, "modified");
            let is_dir = basic_file_attributes_is_dir(ctx, bfa_obj);
            let size = basic_file_attributes_size(ctx, bfa_obj);
            // Slot 5 (new) records the backing path so the DOS flag reads can
            // query the real file instead of answering a hardcoded `false`.
            let dos = try_alloc_concurrent_synthetic(
                ctx,
                "java/nio/file/attribute/DosFileAttributes",
                6,
            )?;
            // Pin across each allocation below — a moving young GC there would
            // relocate the fresh attributes (native stale-local family).
            let dos_pin = ctx.pin_native_root(dos);
            let ct = filetime_alloc(ctx, creation)?;
            let dos_cur = ctx.read_native_pin(dos_pin, dos);
            ctx.set_field(dos_cur, 0, Value::Object(Some(ct)));
            let at = filetime_alloc(ctx, access)?;
            let dos_cur = ctx.read_native_pin(dos_pin, dos);
            ctx.set_field(dos_cur, 1, Value::Object(Some(at)));
            let mt = filetime_alloc(ctx, modified)?;
            let dos_cur = ctx.read_native_pin(dos_pin, dos);
            ctx.set_field(dos_cur, 2, Value::Object(Some(mt)));
            ctx.set_field(dos_cur, 3, Value::Int(i32::from(is_dir)));
            ctx.set_field(dos_cur, 4, Value::Long(size));
            let ps = ctx.create_string(&path);
            let dos_cur = ctx.read_native_pin(dos_pin, dos);
            ctx.set_field(dos_cur, 5, Value::Object(Some(ps)));
            ctx.unpin_native_roots(dos_pin);
            Ok(Some(Value::Object(Some(dos_cur))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/nio/file/attribute/DosFileAttributeView",
        "name",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string("dos"))))), cratonvm_native_api::NativeKind::Bridge,
    );
    // Attribute setters on the views. Gradle marks cache dirs read-only via
    // `getFileAttributeView(dir, DosFileAttributeView.class).setReadOnly(true)`;
    // an unregistered setter → AbstractMethodError aborted the cache-dir setup
    // → the bogus "… is not a directory" leaf (SB-14). No-ops are sufficient
    // (the JDK call only needs to not throw). `setTimes` applies real
    // filesystem timestamps on both views.
    for vclass in [
        "java/nio/file/attribute/BasicFileAttributeView",
        "java/nio/file/attribute/DosFileAttributeView",
        "java/nio/file/attribute/PosixFileAttributeView",
    ] {
        r.register(
            vclass,
            "setTimes",
            "(Ljava/nio/file/attribute/FileTime;Ljava/nio/file/attribute/FileTime;Ljava/nio/file/attribute/FileTime;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let path_value = ctx.get_field(this, 0);
                let path = extract_path_string(ctx, Some(&path_value));
                let modified = args.get(1).and_then(|value| match value {
                    Value::Object(Some(time)) => Some(filetime_read_millis(ctx, *time)),
                    _ => None,
                });
                let access = args.get(2).and_then(|value| match value {
                    Value::Object(Some(time)) => Some(filetime_read_millis(ctx, *time)),
                    _ => None,
                });
                let creation = args.get(3).and_then(|value| match value {
                    Value::Object(Some(time)) => Some(filetime_read_millis(ctx, *time)),
                    _ => None,
                });
                // `Files.setLastModifiedTime(<missing>)` funnels here (it has
                // no `Files`-level native), and a missing file must be
                // `NoSuchFileException`, not the bare `IOException` this
                // mapping produced. MEASURED against HotSpot.
                set_file_attribute_times(&path, creation, access, modified)
                    .map_err(|error| p57_fs_error(ctx, &error, &path))?;
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    //
    // UPDATE (wave-2 stub removal): all four were no-ops, so Gradle's
    // `setReadOnly(true)` on a cache dir reported success and left the
    // directory writable — and a later `readAttributes().isReadOnly()` said
    // so. `setReadOnly` now really chmods/clears the write bits; the three
    // DOS-only flags are applied for real on Windows, and on a non-Windows
    // host a request to SET one now fails loudly instead of pretending
    // (clearing one is a no-op there, because it can never have been set).
    {
        let dos_view = "java/nio/file/attribute/DosFileAttributeView";
        r.register(dos_view, "setReadOnly", "(Z)V", dos_view_set_read_only, cratonvm_native_api::NativeKind::Bridge);
        r.register(dos_view, "setHidden", "(Z)V", dos_view_set_hidden, cratonvm_native_api::NativeKind::Bridge);
        r.register(dos_view, "setSystem", "(Z)V", dos_view_set_system, cratonvm_native_api::NativeKind::Bridge);
        r.register(dos_view, "setArchive", "(Z)V", dos_view_set_archive, cratonvm_native_api::NativeKind::Bridge);
    }
    // PosixFileAttributeView — completes the view returned by
    // `getFileAttributeView(path, PosixFileAttributeView.class)` above.
    // `readAttributes()` reuses `p59_files_read_attributes`, which on a
    // real host path (Linux/macOS) already allocates a *real* JDK
    // `sun/nio/fs/UnixFileAttributes` object (not a synthetic stub) — its
    // own real bytecode implements `permissions()`/`isDirectory()`/etc. by
    // reading the `st_mode` field we populate, so no separate PosixFileAttributes
    // shim is needed here (mirrors the Dos case, which instead re-homes
    // fields into a dedicated synthetic type because `DosFileAttributes`
    // has no such real-class fast path).
    r.register(
        "java/nio/file/attribute/PosixFileAttributeView",
        "readAttributes",
        "()Ljava/nio/file/attribute/PosixFileAttributes;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path_obj = ctx.get_field(this, 0);
            p59_files_read_attributes(ctx, &[path_obj])
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/nio/file/attribute/PosixFileAttributeView",
        "name",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string("posix"))))), cratonvm_native_api::NativeKind::Bridge,
    );
    // `setPermissions` is the operation H2's `FilePathDisk.setReadOnly()` /
    // `TestFileSystem.testSetReadOnly` / `TestTraceSystem.testReadOnly`
    // actually need — chmod the real backing file to the permission bits
    // the caller computed (H2 clears the *_WRITE bits and re-submits the
    // rest). `posix_permission_bits_from_set` walks the same 9 canonical
    // `PosixFilePermission` singletons `PosixFilePermissions.toString`/
    // `fromString` already use.
    r.register(
        "java/nio/file/attribute/PosixFileAttributeView",
        "setPermissions",
        "(Ljava/util/Set;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path_value = ctx.get_field(this, 0);
            let path = extract_path_string(ctx, Some(&path_value));
            let set = obj_arg(args, 1)?;
            let mode = posix_permission_bits_from_set(ctx, set);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .map_err(|error| p57_io_error(&error))?;
            }
            #[cfg(not(unix))]
            {
                let _ = (&path, mode);
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `getOwner`/`setOwner` (from the `FileOwnerAttributeView` supertype) —
    // not exercised by the H2 tests this view was added for, but left
    // unregistered would AbstractMethodError for any other caller now that
    // this view is reachable.
    //
    // `getOwner` was `null`, which `Files.getOwner`'s contract never permits —
    // it throws instead — so every caller NPE'd on the result. There IS a real
    // principal to return: `p59_files_read_attributes` fills `st_uid`, and
    // `native-io` registers `UnixNativeDispatcher.getpwuid`, so the real JDK's
    // own `UnixFileAttributes.owner()` bytecode resolves it. Delegate to it.
    r.register(
        "java/nio/file/attribute/PosixFileAttributeView",
        "getOwner",
        "()Ljava/nio/file/attribute/UserPrincipal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path_value = ctx.get_field(this, 0);
            nio_owner_principal(ctx, path_value)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `setOwner` was a silent no-op that reported success while leaving the
    // file's owner untouched — the worst outcome for a caller doing a
    // permissions handover. There is no portable `chown` behind this VM, so
    // fail honestly instead.
    r.register(
        "java/nio/file/attribute/PosixFileAttributeView",
        "setOwner",
        "(Ljava/nio/file/attribute/UserPrincipal;)V",
        |_ctx, _args| {
            Err(RuntimeError::UnsupportedOperationException {
                message: "PosixFileAttributeView.setOwner is not supported".into(),
            }
            .into())
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // The registration half of the concrete views (JVMS 6.5, nio21 close): the
    // view `getFileAttributeView` mints wears the JDK's own class
    // (`UnixFileAttributeViews$Basic` / `$Posix`, `WindowsFileAttributeViews$Basic`
    // / `$Dos`), and dispatch keys on the receiver's own class first (`H11-1`),
    // so every row above is mirrored onto them. Slot 0 of each is the `file`
    // field, where the carrier kept the Path.
    for (iface, impls) in P57_VIEW_IMPLS {
        for target in *impls {
            cratonvm_native_io::concrete_receiver::mirror_class_registrations(
                r,
                __view_rows_before,
                iface,
                target,
            );
        }
    }
    // DosFileAttributes — same 5-field layout as BasicFileAttributes
    // (creation=0, lastAccess=1, lastMod=2, isDir=3, size=4) plus the four
    // DOS-specific flags (all false in this VM).
    {
        let dfa = "java/nio/file/attribute/DosFileAttributes";
        r.register(
            dfa,
            "creationTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(ctx.get_field(this, 0)))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            dfa,
            "lastAccessTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(ctx.get_field(this, 1)))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            dfa,
            "lastModifiedTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(ctx.get_field(this, 2)))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(dfa, "isDirectory", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(dfa, "isRegularFile", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is_dir = matches!(ctx.get_field(this, 3), Value::Int(1));
            Ok(Some(Value::Int(if is_dir { 0 } else { 1 })))
        }, cratonvm_native_api::NativeKind::Bridge);
        // The five predicates below were hardcoded `false` — "no file this VM
        // ever describes is a symlink, read-only, hidden, archived or a system
        // file". `readAttributes` above now records the backing path in slot
        // 5, so each one can answer from the real file. A caller that skipped
        // a read-only file, or re-processed a hidden one, was being told the
        // wrong thing on every single query.
        r.register(dfa, "isSymbolicLink", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            let link = !p.is_empty() && std::path::Path::new(&p).is_symlink();
            Ok(Some(Value::Int(i32::from(link))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Was a hardcoded `false` on the theory that "nothing in this VM's Path
        // surface can name a device/socket/FIFO". That is not true — a `Path`
        // is just a string, and `Files.readAttributes("/dev/null")` or a walk
        // over `/dev`, `/proc` or a unix-socket directory reaches here. The
        // real body (`UnixFileAttributes.isOther()` /
        // `WindowsFileAttributes.isOther()`) is
        // `!isRegularFile() && !isDirectory() && !isSymbolicLink()`; slot 5
        // carries the backing path (see `readAttributes` above), so apply that
        // definition to the real file type.
        r.register(dfa, "isOther", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            if p.is_empty() {
                return Ok(Some(Value::Int(0)));
            }
            let other = match std::fs::symlink_metadata(&p) {
                Ok(md) => {
                    let ft = md.file_type();
                    !ft.is_file() && !ft.is_dir() && !ft.is_symlink()
                }
                Err(_) => false,
            };
            Ok(Some(Value::Int(i32::from(other))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(dfa, "size", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 4)))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Was an unconditional `null`, so `FileTreeWalker.wouldLoop` could never
        // detect a symlink cycle and no two paths could ever be recognised as
        // the same file. `readAttributes` records the backing path in slot 5 —
        // ask the OS for the real identity (see `file_identity_key_for_path`);
        // `null` survives only as the genuine "unavailable" answer.
        r.register(dfa, "fileKey", "()Ljava/lang/Object;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            match file_identity_key_for_path(&p) {
                Some(key) => {
                    let s = ctx.create_string(&key);
                    Ok(Some(Value::Object(Some(s))))
                }
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(dfa, "isReadOnly", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            let ro = !p.is_empty()
                && std::fs::metadata(&p)
                    .map(|m| m.permissions().readonly())
                    .unwrap_or(false);
            Ok(Some(Value::Int(i32::from(ro))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(dfa, "isHidden", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            Ok(Some(Value::Int(i32::from(dos_attr_flag(
                &p,
                DOS_ATTR_HIDDEN,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(dfa, "isArchive", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            Ok(Some(Value::Int(i32::from(dos_attr_flag(
                &p,
                DOS_ATTR_ARCHIVE,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(dfa, "isSystem", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            Ok(Some(Value::Int(i32::from(dos_attr_flag(
                &p,
                DOS_ATTR_SYSTEM,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge);
    }

    // FileSystemProvider.getFileStore(Path) — abstract on the base, so the
    // synthetic default provider (stamped as the literal
    // `java/nio/file/spi/FileSystemProvider` class, not a concrete
    // sun.nio.fs.* subclass) lacked it: `Files.getFileStore(path)`'s real
    // bytecode is `return provider(path).getFileStore(path);`, and the
    // `provider()` native above returns exactly that synthetic instance, so
    // the invokevirtual landed on the abstract declaration —
    // "AbstractMethodError: FileSystemProvider.getFileStore(Path) has no
    // Code attribute" — killing any Elasticsearch/Lucene engine test whose
    // constructor path calls `Environment.getFileStore(...)` (8 CratonVM-only
    // suite failures incl. InternalEngineFieldInfoCachingTests,
    // ReadOnlyEngineTests, NoOpEngineTests). Return a synthetic `FileStore`
    // (see below) rather than null — the JDK contract never returns null
    // here, and ES's `ESFileStore` wrapper unconditionally calls through to
    // `in.getTotalSpace()`/`in.isReadOnly()`/etc. on the result.
    r.register(
        fsp,
        "getFileStore",
        "(Ljava/nio/file/Path;)Ljava/nio/file/FileStore;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "getFileStore",
                "(Ljava/nio/file/Path;)Ljava/nio/file/FileStore;"
            );
            let mut path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, &mut path_obj);
            // `Files.getFileStore` has no `Files`-level native, so its real
            // bytecode calls straight through to HERE — and this copy lacked
            // the existence check its `Files`-level twin already carried.
            // MEASURED: `Files.getFileStore(<missing>)` answered a FileStore
            // where HotSpot raises `NoSuchFileException`. The half-fixed
            // duplicate pair again: the fix landed on the copy nothing runs.
            if std::fs::symlink_metadata(&p).is_err() {
                return Err(p57_no_such_file(ctx, &p)?);
            }
            Ok(Some(Value::Object(Some(p57_alloc_file_store(ctx, &p)?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // java.nio.file.FileStore — every accessor is abstract in the real JDK
    // (delegated to sun.nio.fs.{Windows,Unix}FileStore in a real install),
    // so an instance stamped as the literal abstract `FileStore` class needs
    // every one of them registered directly or each individual accessor call
    // throws its own "has no Code attribute" AbstractMethodError in turn.
    // Field 0 = backing path string (drives name()/type()); values otherwise
    // mirror the `java/io/File` disk-space fallback below (`getTotalSpace`
    // etc. — no portable free-space query, so report "plenty available").
    {
        // Every accessor below reads the store's ONE private slot through
        // `file_store_base`, not at a literal 0: since 2026-08-21
        // `p57_alloc_file_store` mints the concrete `sun.nio.fs.*FileStore`
        // (JVMS 6.5, `FILE_STORE_IMPLS`), whose own declared fields occupy the
        // low slots. The base collapses to 0 for any receiver this crate did
        // not allocate, so a foreign or stub-mode store reads what it read
        // before.
        let fs_store = "java/nio/file/FileStore";
        let __fs_rows_before = r.dump_registrations().len();
        r.register(fs_store, "name", "()Ljava/lang/String;", |ctx, args| {
            retired_receiver_yield!(ctx, args, "name", "()Ljava/lang/String;");
            let this = obj_arg(args, 0)?;
            let base = file_store_base(ctx, this);
            let name = match ctx.get_field(this, base) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(Value::Object(Some(ctx.create_string(&name)))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(fs_store, "type", "()Ljava/lang/String;", |ctx, args| {
            retired_receiver_yield!(ctx, args, "type", "()Ljava/lang/String;");
            let t = if cfg!(windows) { "NTFS" } else { "ext4" };
            Ok(Some(Value::Object(Some(ctx.create_string(t)))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Was a hardcoded `false`, justified as "there is no portable
        // read-only-MOUNT query". There is one on each platform, and it is the
        // same query the JDK itself issues: `UnixFileStore.isReadOnly()` tests
        // `ST_RDONLY` in the `statvfs`/mount flags, and `WindowsFileStore.
        // isReadOnly()` tests `FILE_READ_ONLY_VOLUME` from
        // `GetVolumeInformation`. Field 0 holds the store's mount root — the
        // same field `getBlockSize`/`getTotalSpace` already probe. A read-only
        // mount (a loop-mounted image, a CD, a container's `ro` bind mount)
        // was previously reported as writable to every caller that pre-checks.
        r.register(fs_store, "isReadOnly", "()Z", |ctx, args| {
            retired_receiver_yield!(ctx, args, "isReadOnly", "()Z");
            let this = obj_arg(args, 0)?;
            let base = file_store_base(ctx, this);
            let path = match ctx.get_field(this, base) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let ro = file_store_is_read_only(&path).unwrap_or(false);
            Ok(Some(Value::Int(i32::from(ro))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // These three were `Long::MAX_VALUE` — "infinite disk". Any caller
        // doing capacity planning (ES's disk-threshold allocation decider,
        // Spring Boot's `DiskSpaceHealthIndicator`, an installer's free-space
        // pre-check) got a fabricated answer and could never report a full
        // disk. `file_disk_space_bytes` is the same real query the
        // `java.io.File` disk-space natives already use; field 0 holds the
        // store's mount root.
        r.register(fs_store, "getTotalSpace", "()J", |ctx, args| {
            retired_receiver_yield!(ctx, args, "getTotalSpace", "()J");
            Ok(Some(Value::Long(file_store_space(ctx, args, 0))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(fs_store, "getUsableSpace", "()J", |ctx, args| {
            retired_receiver_yield!(ctx, args, "getUsableSpace", "()J");
            Ok(Some(Value::Long(file_store_space(ctx, args, 2))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(fs_store, "getUnallocatedSpace", "()J", |ctx, args| {
            retired_receiver_yield!(ctx, args, "getUnallocatedSpace", "()J");
            Ok(Some(Value::Long(file_store_space(ctx, args, 1))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Real JDK's `FileStore.getBlockSize()` default body unconditionally
        // throws `UnsupportedOperationException` (only OS-specific subclasses
        // override it) — ES's `FsDirectoryFactory.blockSize` calls this
        // directly, so give a real answer instead of matching that throw.
        //
        // It was a hardcoded 4096, which is a guess: `FsDirectoryFactory` uses
        // the value to decide whether the store is a rotational disk needing
        // `preload`/`MMapDirectory` tuning, and a 512-byte-sector or 64 KiB-
        // cluster volume was silently described as a 4 KiB one. Field 0 holds
        // the store's mount root, so ask the volume (same query the JDK's own
        // `Unix/WindowsFileStore` issues) and keep 4096 only as the fallback.
        r.register(fs_store, "getBlockSize", "()J", |ctx, args| {
            retired_receiver_yield!(ctx, args, "getBlockSize", "()J");
            let this = obj_arg(args, 0)?;
            let base = file_store_base(ctx, this);
            let path = match ctx.get_field(this, base) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let block = file_store_block_size(&path).unwrap_or(4096);
            Ok(Some(Value::Long(block as i64)))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Both overloads used to answer `true` for EVERY view — including
        // `acl` on Linux and `posix` on Windows, neither of which this VM can
        // serve. A caller that pre-checks with `supportsFileAttributeView`
        // and then calls `getFileAttributeView` therefore got a null view (or
        // an `UnsupportedOperationException`) it had explicitly guarded
        // against. Answer from the same per-platform list
        // `FileSystem.supportedFileAttributeViews()` reports.
        r.register(
            fs_store,
            "supportsFileAttributeView",
            "(Ljava/lang/Class;)Z",
            |ctx, args| {
                retired_receiver_yield!(
                    ctx,
                    args,
                    "supportsFileAttributeView",
                    "(Ljava/lang/Class;)Z"
                );
                let supported = obj_arg(args, 1)
                    .ok()
                    .and_then(|m| crate::lang_class::mirror_class_name(ctx, m))
                    .as_deref()
                    .and_then(attribute_view_short_name)
                    .map(|n| supported_attribute_view_names().contains(&n))
                    .unwrap_or(false);
                Ok(Some(Value::Int(i32::from(supported))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            fs_store,
            "supportsFileAttributeView",
            "(Ljava/lang/String;)Z",
            |ctx, args| {
                retired_receiver_yield!(
                    ctx,
                    args,
                    "supportsFileAttributeView",
                    "(Ljava/lang/String;)Z"
                );
                let name = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => return Ok(Some(Value::Int(0))),
                };
                let supported = supported_attribute_view_names().contains(&name.as_str());
                Ok(Some(Value::Int(i32::from(supported))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        // KEEP the `null`, with the real-JDK anchor: BOTH concrete stores ship
        // this body verbatim —
        //   public <V extends FileStoreAttributeView> V
        //          getFileStoreAttributeView(Class<V> view) {
        //       if (view == null) throw new NullPointerException();
        //       return (V) null;
        //   }
        // (`sun.nio.fs.UnixFileStore` and `sun.nio.fs.WindowsFileStore`), i.e.
        // the JDK itself supports no `FileStoreAttributeView` on any platform.
        // The one piece that WAS missing is the null-argument check, which the
        // spec makes observable; add it so the two agree completely.
        r.register(
            fs_store,
            "getFileStoreAttributeView",
            "(Ljava/lang/Class;)Ljava/nio/file/attribute/FileStoreAttributeView;",
            |ctx, args| {
                retired_receiver_yield!(
                    ctx,
                    args,
                    "getFileStoreAttributeView",
                    "(Ljava/lang/Class;)Ljava/nio/file/attribute/FileStoreAttributeView;"
                );
                if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("FileStore.getFileStoreAttributeView: null view".into()),
                    }
                    .into());
                }
                Ok(Some(Value::Object(None)))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        // AN UNCONDITIONAL REFUSAL, over a measurement this class already
        // has. `getTotalSpace()`/`getUsableSpace()`/`getUnallocatedSpace()`
        // are registered thirty lines up and answer correctly; the
        // string-keyed door -- which is how `FileStore` is specified to expose
        // the same three, and the only way to reach an attribute generically --
        // threw `UnsupportedOperationException` for every name including those.
        //
        //   fs.getTotalSpace()             a real byte count
        //   fs.getAttribute("totalSpace")  UnsupportedOperationException
        //
        // MEASURED with `L4BridgeSweep.java`, where it also ENDED
        // THE RUN: the row was a value row, the throw was uncaught, and the
        // last 14 rows of a different family never executed. The typed
        // accessors are the control that identifies the door rather than the
        // measurement.
        //
        // A null name is an NPE (`Objects.requireNonNull` in the JDK), not a
        // refusal -- reporting UOE there tells a caller the attribute is
        // unsupported when what happened is that they passed nothing.
        r.register(
            fs_store,
            "getAttribute",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            |ctx, args| {
                retired_receiver_yield!(
                    ctx,
                    args,
                    "getAttribute",
                    "(Ljava/lang/String;)Ljava/lang/Object;"
                );
                let name_obj = match args.get(1) {
                    Some(Value::Object(Some(s))) => *s,
                    _ => {
                        return Err(RuntimeError::NullPointerException { message: None }.into());
                    }
                };
                let spec = ctx.read_string(name_obj).unwrap_or_default();
                // `"basic:totalSpace"` and `"totalSpace"` name the same thing;
                // the view prefix is optional and `basic` is the default.
                let attr = match spec.split_once(':') {
                    Some(("basic", rest)) => rest,
                    Some(_) => return Err(unsupported_file_store_attribute(&spec)),
                    None => spec.as_str(),
                };
                let which = match attr {
                    "totalSpace" => 0,
                    "unallocatedSpace" => 1,
                    "usableSpace" => 2,
                    _ => return Err(unsupported_file_store_attribute(&spec)),
                };
                let space = file_store_space(ctx, args, which);
                Ok(Some(box_long(ctx, space)))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(fs_store, "toString", "()Ljava/lang/String;", |ctx, args| {
            retired_receiver_yield!(ctx, args, "toString", "()Ljava/lang/String;");
            let this = obj_arg(args, 0)?;
            let base = file_store_base(ctx, this);
            let name = match ctx.get_field(this, base) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let t = if cfg!(windows) { "NTFS" } else { "ext4" };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&format!("{name} ({t})")),
            ))))
        }, cratonvm_native_api::NativeKind::Bridge);

        // The registration half. Dispatch keys on the receiver's runtime class
        // (`H11-1`), and `sun.nio.fs.UnixFileStore` declares `name`, `type`,
        // `isReadOnly`, `getTotalSpace`, … with `Code` -- so moving the mint
        // without moving these rows would hand every accessor to the JDK's own
        // body against a store whose `<init>` never ran.
        for target in FILE_STORE_IMPLS {
            cratonvm_native_io::concrete_receiver::mirror_class_registrations(
                r,
                __fs_rows_before,
                fs_store,
                target,
            );
        }
    }

    r.register(
        fsp,
        "installedProviders",
        "()Ljava/util/List;",
        |ctx, _args| {
            use cratonvm_types::ArrayElementType;
            // The jar/jrt entries stay per-call allocations (see the residual
            // note in W8-C14-2); `p57_alloc_provider` is now the single
            // implementation, replacing a byte-identical private `mk_provider`
            // closure that used to live here.
            //
            // "jrt" lets `FileSystems.getFileSystem(URI.create("jrt:/"))` resolve
            // (the real-JDK static iterates installedProviders by scheme) so the
            // in-process compiler can read platform classes from the runtime
            // image (HIB-CV-27).
            //
            // Element 0 MUST BE the very object `FileSystems.getDefault()
            // .provider()` returns: the `FileSystemProvider.installedProviders`
            // javadoc makes the default provider the first element of this
            // list, and HotSpot answers
            // `installedProviders.get0==getDefault.provider|true` (measured,
            // scratchpad/c14/FsIdentity.java). This was `mk_provider(ctx,
            // "file")` — a fresh object per call — so the identity was false,
            // and false again between two consecutive calls to this method.
            let file_p = p57_default_provider_singleton(ctx)?;
            let file_pin = ctx.pin_native_root(file_p);
            let jar_p = p57_alloc_provider(ctx, "jar")?;
            let jar_pin = ctx.pin_native_root(jar_p);
            let jrt_p = p57_alloc_provider(ctx, "jrt")?;
            let jrt_pin = ctx.pin_native_root(jrt_p);
            let arr = ctx.new_array(ArrayElementType::Reference, 3);
            let arr_pin = ctx.pin_native_root(arr);
            let file_p = ctx.read_native_pin(file_pin, file_p);
            let jar_p = ctx.read_native_pin(jar_pin, jar_p);
            let jrt_p = ctx.read_native_pin(jrt_pin, jrt_p);
            ctx.set_array_element(arr, 0, Value::Object(Some(file_p)));
            ctx.set_array_element(arr, 1, Value::Object(Some(jar_p)));
            ctx.set_array_element(arr, 2, Value::Object(Some(jrt_p)));
            // Real ArrayList field layout in real-JDK mode:
            //   [0]=AbstractList.modCount (int), [1]=elementData (Object[]), [2]=size (int).
            // gc-common w15-f: typed so the stale-handle audit sees the binding;
            // this allocation is the last GC point before the stores below.
            let list: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            // By NAME, not by raw slot. These three writes were
            // `set_field(list, 0/1/2, ..)` against the REAL JDK layout
            // (`AbstractList.modCount`, `ArrayList.elementData`,
            // `ArrayList.size`) -- an absolute extent of 3 on a class this
            // code does not own, which is what the T9C gate scores against the
            // synthetic table's 2 and what that gate's own "rule when this
            // fails" says to replace. Resolving by name is layout-independent:
            // correct here, and in synthetic mode a write that finds no such
            // field is dropped rather than landing on the wrong slot.
            ctx.set_field_by_name(list, "modCount", Value::Int(0));
            ctx.set_field_by_name(list, "elementData", Value::Object(Some(arr)));
            ctx.set_field_by_name(list, "size", Value::Int(3));
            ctx.unpin_native_roots(file_pin);
            Ok(Some(Value::Object(Some(list))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        fsp,
        "getFileSystem",
        "(Ljava/net/URI;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "getFileSystem",
                "(Ljava/net/URI;)Ljava/nio/file/FileSystem;"
            );
            // `FileSystems.getFileSystem(uri)` matches an installed provider by
            // scheme then invokes this on it, so the scheme is on `this` (field
            // 0); also inspect the URI for robustness. A jrt: URI yields the
            // runtime-image FileSystem (HIB-CV-27 in-process javac).
            let scheme = obj_arg(args, 0)
                .ok()
                .and_then(|this| match ctx.get_field(this, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                })
                .unwrap_or_default();
            let uri_is_jrt = match args.get(1) {
                Some(Value::Object(Some(u))) => p57_uri_full_text(ctx, *u).starts_with("jrt:"),
                _ => false,
            };
            if scheme == "jrt" || uri_is_jrt {
                let jh = ctx.get_system_property("java.home").unwrap_or_default();
                let fs = p57_alloc_jrt_filesystem(ctx, &jh)?;
                return Ok(Some(Value::Object(Some(fs))));
            }
            // The default filesystem, NOT a fresh one. `FileSystems
            // .getFileSystem(URI.create("file:///"))` resolves the "file"
            // provider and calls this; HotSpot returns the same object as
            // `getDefault()` (measured: `getFileSystem(file:///)==getDefault
            // |true`). `p57_alloc_default_filesystem` here handed back a second
            // filesystem — the same two-doors defect as
            // `DefaultFileSystemProvider.theFileSystem`, one level down, and
            // `FileSystems$DefaultFileSystemHolder.getDefaultFileSystem()`
            // reaches it via exactly this call
            // (`provider.getFileSystem(URI.create("file:///"))`).
            let fs = p57_default_filesystem_singleton(ctx)?;
            Ok(Some(Value::Object(Some(fs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        fsp,
        "getPath",
        "(Ljava/net/URI;)Ljava/nio/file/Path;",
        |ctx, args| {
            retired_receiver_yield!(ctx, args, "getPath", "(Ljava/net/URI;)Ljava/nio/file/Path;");
            let uri = obj_arg(args, 1)?;
            // Opaque file-scheme URIs (`file:.`) are not hierarchical — the
            // real JDK's *UriSupport.fromUri throws instead of producing a
            // path. (Spring's PathEditor depends on this throw.)
            let uri_text = p57_uri_full_text(ctx, uri);
            if p57_uri_is_opaque_file(&uri_text) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "URI is not hierarchical".to_string(),
                }
                .into());
            }
            // `Paths.get(uri)` real bytecode dispatches non-`file` schemes to
            // `FileSystemProvider.getPath(uri)` on the matching installed
            // provider — for a `jar:` URI (e.g. from `URL.toURI()` on a
            // `getResources()` hit inside a jar) that lands here, not in
            // `Path.of(URI)`. Mirror that native's jar-aware handling so both
            // entry points mount the same jar-backed Path/FileSystem instead
            // of this falling through to the generic field-4/field-0 read
            // below, which only understands `file:` URIs and previously
            // produced a garbage single-segment path (e.g. just "jar", the
            // bare scheme) for jar-backed lookups — see `Resources.addPackage`
            // in spring-boot-test-support, which resolves `test.jks` etc. via
            // exactly this path.
            if let Some((jar, entry)) = p57_jar_uri_to_entry_path(&uri_text) {
                // gc-common w21-b: the FileSystem is rooted across the Path's
                // allocation and stored through its current address.
                let fs = p57_alloc_jar_filesystem(ctx, &jar)?;
                let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                let fs_h = scope.root(fs);
                let result = p57_alloc_path(&mut *scope, &jarfs_encode(&jar, &entry))?;
                let fs = scope.get(&fs_h);
                scope.set_field(result, p57_path_slots_for(scope.vm_identity()).fs, Value::Object(Some(fs)));
                return Ok(Some(Value::Object(Some(result))));
            }
            // `URI.getPath()` first -- the DECODED path component (see the
            // matching note in the `Path.of(URI)` native above); the field
            // probes below all yield the RAW, still percent-encoded form.
            //
            // gen r4w3/rooting: `getPath()` is Java (a GC point) and the field
            // probes below read `uri` again, so read them through the pinned,
            // current address.
            let uri_pin = ctx.pin_native_root(uri);
            let decoded_path = ctx.invoke_virtual(uri, "getPath", "()Ljava/lang/String;", &[]);
            let uri = ctx.read_native_pin(uri_pin, uri);
            ctx.unpin_native_roots(uri_pin);
            if let Ok(Some(Value::Object(Some(s)))) = decoded_path {
                if let Some(decoded) = ctx.read_string(s) {
                    if !decoded.is_empty() {
                        let os_path = p57_to_os_path(&decoded);
                        let result = p57_alloc_path(ctx, &os_path)?;
                        return Ok(Some(Value::Object(Some(result))));
                    }
                }
            }
            // URI field 4 is the path component (from our toUri registration)
            let path_str = match ctx.get_field(uri, 4) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => match ctx.get_field(uri, 0) {
                    Value::Object(Some(s)) => {
                        let raw = ctx.read_string(s).unwrap_or_default();
                        raw.strip_prefix("file://").unwrap_or(&raw).to_string()
                    }
                    _ => String::new(),
                },
            };
            let os_path = p57_to_os_path(&path_str);
            let result = p57_alloc_path(ctx, &os_path)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Keycloak PersistedConfigSource.loadPersistedConfig ---
    // CratonVM divergence: on Windows this method opens
    // lib/quarkus/generated-bytecode.jar via FileInputStream + ZipInputStream
    // and iterates entries looking for META-INF/keycloak-persisted.properties.
    // The jar shipped with Keycloak 26.6.1 does not contain that entry, and
    // somewhere during the entry walk CratonVM's IO/zip pipeline hits an
    // EOFException in ZipInputStream.readFully (readLOC -> getNextEntry).
    // The exception escapes loadPersistedConfig (it returns InputStream, not
    // wrapped) and is rethrown by readProperties() as RuntimeException
    // "Failed to load persisted properties.", which Keycloak's
    // AbstractAutoBuildCommand wraps into picocli ExecutionException
    // "Failed to update server configuration." aborting start-dev.
    // Returning null is the spec-compliant "no persisted config" answer
    // (readProperties() then returns Collections.emptyMap()).
    //
    // WAVE-4 REACHABILITY CORRECTION, AMENDED 2026-08-04 — the original note
    // said `register_phase57_nio_file` is reached only from
    // `register_phase57_natives` -> `register_synthetic_overrides` (which is
    // `#[cfg(feature = "synthetic-jdk")]` and never called by the default
    // real-JDK CLI), and therefore that this whole function is inert in
    // real-JDK mode. THAT IS FALSE, and it cost a later session an hour of
    // chasing the live `Path` implementation into `native-io` (verified by
    // instrumenting both allocators and watching which one fires): `vm/src/vm/
    // vm_init.rs` calls `cratonvm_native_builtins::phases_late::
    // register_phase57_nio_file` DIRECTLY, in both the synthetic and the
    // real-JDK arm (vm_init.rs:2217 and :2763 as of 2026-08-12 — the two cited
    // here had rotted by ~480 lines, so re-read them rather than trusting
    // either pair). Everything registered in this
    // function — Path/Paths/Files, the p57 allocator — is live in the shipping
    // CLI, and it OVERRIDES `native-io`'s same-key registrations. What IS
    // synthetic-only is the phase-61 block (`register_p61_files_path`), which
    // vm_init never calls in real-JDK mode. This specific shim still cannot be
    // what makes `start-dev` get past the EOFException today, and it is not
    // currently papering over the `ZipInputStream.readFully`/`readLOC` bug —
    // in real-JDK mode that bug (if still present) is reached through real JDK
    // `ZipInputStream` bytecode, with no native of ours in the path. Left in
    // place for the `--synthetic-jdk` lane; the zip defect is escalated
    // separately rather than being treated as covered here.
    r.register(
        "org/keycloak/quarkus/runtime/configuration/PersistedConfigSource",
        "loadPersistedConfig",
        "()Ljava/io/InputStream;",
        |_ctx, _args| Ok(Some(Value::Object(None))), cratonvm_native_api::NativeKind::Bridge,
    );

    // `ConfigSourceContextConfigSource.getPropertyNames()` assumes its context
    // iterator is String-only. A stale collection placeholder must never leak
    // across that boundary: SmallRye uses this adapter while building nested
    // keystore mappings, and its bytecode otherwise throws a CCE before it can
    // validate the actual configured names. Preserve all genuine String names
    // and ignore only invalid non-String entries.
    r.register(
        "io/smallrye/config/ConfigSourceContext$ConfigSourceContextConfigSource",
        "getPropertyNames",
        "()Ljava/util/Set;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let context = match ctx.get_field(this, 0) {
                Value::Object(Some(o)) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let this_pin = ctx.pin_native_root(this);
            let context_pin = ctx.pin_native_root(context);
            let set = match ctx.new_object_initialized("java/util/HashSet", "()V", &[]) {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => {
                    ctx.unpin_native_roots(this_pin);
                    ctx.unpin_native_roots(context_pin);
                    return Ok(Some(Value::Object(None)));
                }
            };
            let set_pin = ctx.pin_native_root(set);
            let context = ctx.read_native_pin(context_pin, context);
            let iterator =
                match ctx.invoke_virtual(context, "iterateNames", "()Ljava/util/Iterator;", &[]) {
                    Ok(Some(Value::Object(Some(o)))) => o,
                    _ => {
                        ctx.unpin_native_roots(this_pin);
                        ctx.unpin_native_roots(context_pin);
                        ctx.unpin_native_roots(set_pin);
                        return Ok(Some(Value::Object(Some(set))));
                    }
                };
            let iterator_pin = ctx.pin_native_root(iterator);
            loop {
                let iterator = ctx.read_native_pin(iterator_pin, iterator);
                let more = ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])?;
                if !matches!(more, Some(Value::Int(v)) if v != 0) {
                    break;
                }
                let iterator = ctx.read_native_pin(iterator_pin, iterator);
                let value = ctx.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])?;
                if let Some(Value::Object(Some(value))) = value {
                    if ctx
                        .class_name_arc_of_id(ctx.class_id_of_object(value))
                        .as_deref()
                        == Some("java/lang/String")
                    {
                        let value_pin = ctx.pin_native_root(value);
                        let set = ctx.read_native_pin(set_pin, set);
                        let value = ctx.read_native_pin(value_pin, value);
                        let _ = ctx.invoke_virtual(
                            set,
                            "add",
                            "(Ljava/lang/Object;)Z",
                            &[Value::Object(Some(value))],
                        );
                        ctx.unpin_native_roots(value_pin);
                    }
                }
            }
            let set = ctx.read_native_pin(set_pin, set);
            ctx.unpin_native_roots(this_pin);
            ctx.unpin_native_roots(context_pin);
            ctx.unpin_native_roots(iterator_pin);
            ctx.unpin_native_roots(set_pin);
            Ok(Some(Value::Object(Some(set))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Keycloak's legacy Config.Scope consumer calls this concrete one-argument
    // method. Route it through the inherited two-argument lookup so config
    // resolution follows the same path as the JDK implementation.
    (__fs_rows_before, __fsp_rows_before)
}
