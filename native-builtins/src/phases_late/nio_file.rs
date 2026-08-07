// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.nio.file` natives: Path/Paths/Files, FileSystem providers (default, jar, jrt), file attributes, FileVisitor/WatchService, RandomAccessFile/File/FileChannel.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// ============================================================================
// Phase 57: NIO File extras (Path/Paths/Files), ProcessBuilder/Process,
//           java.text (DecimalFormat, SimpleDateFormat, MessageFormat)
// ============================================================================

pub(crate) fn register_phase57_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_phase57_nio_file(registry);
    register_phase57_process(registry);
    register_phase57_text(registry);
    // Real-RAF is now the DEFAULT (mirrors the native-io gate in
    // `register_io_extras_natives`). The high-level synthetic RAF natives are
    // skipped so RandomAccessFile runs real JDK bytecode + the open0/read0/seek0/
    // length0 primitives; the two synthetic impls otherwise conflict and leave
    // `this.fd` null (length()=0/read()=-1). Both crates must skip together — each
    // shadows the real ctor via native-override priority. Opt back into synthetic
    // with CRATONVM_SYNTHETIC_RAF=1. (SEGV/Cleaner crashes that once gated this are
    // fixed: app-jvm-bugs/real-raf-segv-root-cause.md.)
    if crate::vmflags().io.synthetic_raf_forced {
        register_phase57_random_access_file(registry);
    }
    register_phase57_file(registry);
    register_phase57_file_channel(registry);
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// java.nio.file — Path extras, Files extras, FileVisitResult, OpenOption
// Path = 1-field synthetic (field 0 = String path)
// ---------------------------------------------------------------------------
pub(crate) const P57_PATH_FIELD: usize = 0;

/// Field index on a synthetic `java/nio/file/Path` that holds the
/// `FileSystem` object the path was created from. `Path.getFileSystem()`
/// returns this so callers that identity-compare
/// `path.getFileSystem() == FileSystems.getDefault()` — e.g. cassandra's
/// `org.apache.cassandra.io.util.File` constructor — observe a match.
/// Null when the path was created without a known owning FileSystem.
pub(crate) const P57_PATH_FS_FIELD: usize = 1;

/// Field index on a synthetic `java/nio/file/FileSystem` that, when set, holds
/// the OS path of a mounted JAR (see `newFileSystem`). Field 0 is the separator.
pub(crate) const P57_FS_JAR_FIELD: usize = 1;

/// Field index on a synthetic `java/nio/file/FileSystem` that, when set, holds
/// the `java.home` of a mounted runtime-image (`jrt:`) filesystem. Field 0 is
/// the separator, field 1 the mounted-JAR path; these are mutually exclusive.
pub(crate) const P57_FS_JRT_FIELD: usize = 2;

/// Native equivalent of `java.nio.file.Path.toString()`, tuned for synthetic
/// and real `java/nio/file/Path` values used by Javac/ZipFS and JRT paths.
/// This avoids going back through virtual `Path.toString` dispatch, keeping
/// file-bridge paths and archive entry identity stable for real-JDK callers.
pub fn p57_path_display_string(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let p = p57_read_path(ctx, this);
    match vfs_decode(&p) {
        // jar-FS / jrt-FS Path.toString() shows the in-archive entry with
        // '/' (matches the JDK zipfs/jrtfs separator), regardless of host OS.
        Some((_, _, e)) => {
            if e.starts_with('/') {
                e
            } else {
                format!("/{e}")
            }
        }
        // A plain (non-encoded) relative path whose owning FileSystem is a
        // virtual (jar/jrt) FS renders with '/' — e.g. the result of
        // `jarRoot.relativize(dir)` ("org/h2/tools"), which javac turns into
        // a package name. Rendering the host '\' there would corrupt the key.
        None if path_owned_by_virtual_fs(ctx, this) => p.replace('\\', "/"),
        // Host-FS path: render the OS-native separator. CratonVM stores
        // paths with '/' internally, but HotSpot's WindowsPath.toString()
        // renders '\'; convert at this display boundary on Windows
        // (no-op on Unix). Matches `File.getPath()` below.
        None => file_normalise_path(&p),
    }
}

pub fn register_phase57_nio_file(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let path = "java/nio/file/Path";
    let _paths = "java/nio/file/Paths";
    let files = "java/nio/file/Files";

    // --- Path extras ---
    r.register(
        path,
        "toAbsolutePath",
        "()Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, this);
            let abs = p57_absolute_path_string(&p);
            let result = p57_alloc_path(ctx, &abs);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    r.register(path, "normalize", "()Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let normalized = p57_normalize_path(&p);
        let result = p57_alloc_path(ctx, &normalized);
        Ok(Some(Value::Object(Some(result))))
    });

    r.register(path, "startsWith", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other_ref = obj_arg(args, 1)?;
        let p = p57_read_path(ctx, this);
        let other = ctx.read_string(other_ref).unwrap_or_default();
        Ok(Some(Value::Int(if p.starts_with(&other) { 1 } else { 0 })))
    });

    r.register(path, "endsWith", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other_ref = obj_arg(args, 1)?;
        let p = p57_read_path(ctx, this);
        let other = ctx.read_string(other_ref).unwrap_or_default();
        Ok(Some(Value::Int(if p.ends_with(&other) { 1 } else { 0 })))
    });

    r.register(
        path,
        "startsWith",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let p1 = p57_read_path(ctx, this);
            let p2 = p57_read_path(ctx, other);
            Ok(Some(Value::Int(if p1.starts_with(&p2) { 1 } else { 0 })))
        },
    );

    r.register(path, "endsWith", "(Ljava/nio/file/Path;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let p1 = p57_read_path(ctx, this);
        let p2 = p57_read_path(ctx, other);
        Ok(Some(Value::Int(if p1.ends_with(&p2) { 1 } else { 0 })))
    });

    r.register(path, "getNameCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let count = p57_name_elements(&p).len() as i32;
        Ok(Some(Value::Int(count)))
    });

    r.register(path, "getName", "(I)Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let p = p57_read_path(ctx, this);
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
        let result = p57_alloc_path(ctx, name);
        Ok(Some(Value::Object(Some(result))))
    });

    r.register(path, "subpath", "(II)Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let begin = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let end = match args[2] {
            Value::Int(i) => i,
            _ => 0,
        };
        let p = p57_read_path(ctx, this);
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
        let result = p57_alloc_path(ctx, &sub.join("/"));
        Ok(Some(Value::Object(Some(result))))
    });

    r.register(path, "toFile", "()Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let file = alloc_concurrent_synthetic(ctx, "java/io/File", 1);
        // Pin across the create_string below — a moving young GC there would
        // relocate the fresh File (native stale-local family).
        let file_pin = ctx.pin_native_root(file);
        let s = ctx.create_string(&p);
        let file = ctx.read_native_pin(file_pin, file);
        ctx.set_field(file, 0, Value::Object(Some(s)));
        ctx.unpin_native_roots(file_pin);
        Ok(Some(Value::Object(Some(file))))
    });

    r.register(path, "toUri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
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
        let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 7);
        let raw = ctx.create_string(&uri_str);
        ctx.set_field(uri, 0, Value::Object(Some(raw)));
        let scheme = ctx.create_string("file");
        ctx.set_field(uri, 1, Value::Object(Some(scheme)));
        let path_str = ctx.create_string(&abs);
        ctx.set_field(uri, 4, Value::Object(Some(path_str)));
        Ok(Some(Value::Object(Some(uri))))
    });

    r.register(path, "compareTo", "(Ljava/nio/file/Path;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let p1 = p57_read_path(ctx, this);
        let p2 = p57_read_path(ctx, other);
        Ok(Some(Value::Int(p1.cmp(&p2) as i32)))
    });

    r.register(path, "isAbsolute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        // keycloak-15: a drive-relative (`C:foo`) or driveless-rooted (`\foo`)
        // path is NOT absolute on Windows — see `p57_win_is_absolute`. On Unix
        // the POSIX rule (leading `/`) applies.
        let abs = if cfg!(windows) {
            p57_win_is_absolute(&p)
        } else {
            p.starts_with('/')
        };
        Ok(Some(Value::Int(if abs { 1 } else { 0 })))
    });

    r.register(path, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let parts: Vec<String> = p57_name_elements(&p);
        use cratonvm_types::ArrayElementType;
        let arr = ctx.new_array(ArrayElementType::Reference, parts.len());
        for (i, part) in parts.iter().enumerate() {
            let path_obj = p57_alloc_path(ctx, part);
            ctx.set_array_element(arr, i, Value::Object(Some(path_obj)));
        }
        let itr = alloc_concurrent_synthetic(ctx, "Path$Itr", 2);
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(itr))))
    });

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
    r.register(
        path,
        "of",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            let first_ref = obj_arg(args, 0)?;
            let first = ctx.read_string(first_ref).unwrap_or_default();
            let mut acc = first;
            if let Some(Value::Object(Some(arr))) = args.get(1) {
                let len = ctx.array_length(*arr);
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                        let part = ctx.read_string(s).unwrap_or_default();
                        if !part.is_empty() {
                            acc = p57_resolve_paths(&acc, &part);
                        }
                    }
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
            let result = p57_alloc_path(ctx, &acc);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // Path.of(URI) — Spring Boot 3.4 fat-jar launcher uses
    // `Path.of(URL.toURI()).toFile()` to convert a `file:/C:/...jar` URL
    // back into a `File`. Read the URI's `path` field via `get_field_by_name`
    // (real-JDK URI declares 16+ fields; using by-name access keeps us
    // independent of layout drift). Fall back to URI's `string` (full URI
    // text) and strip the `file:` scheme prefix when path is unset.
    r.register(
        path,
        "of",
        "(Ljava/net/URI;)Ljava/nio/file/Path;",
        |ctx, args| {
            let uri = obj_arg(args, 0)?;
            let uri_text = p57_uri_full_text(ctx, uri);
            // `Paths.get(jar:file:...!/entry)` is the resource-facing half of
            // the jar-FS contract. Jetty's PathResourceFactory mounts the URI
            // first, then calls this conversion for the root and every
            // resolved child. Treating the full `jar:` text as an ordinary host
            // path loses the mounted archive identity, making Files.isDirectory
            // false and Files.list() empty despite a valid central directory.
            if let Some((jar, entry)) = p57_jar_uri_to_entry_path(&uri_text) {
                let fs = p57_alloc_jar_filesystem(ctx, &jar);
                let result = p57_alloc_path(ctx, &jarfs_encode(&jar, &entry));
                ctx.set_field(result, P57_PATH_FS_FIELD, Value::Object(Some(fs)));
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
            if let Ok(Some(Value::Object(Some(s)))) =
                ctx.invoke_virtual(uri, "getPath", "()Ljava/lang/String;", &[])
            {
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
            let result = p57_alloc_path(ctx, &os_path);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // --- Path.getFileSystem() → the FileSystem this Path belongs to ---
    // FileSystem = 2-field synthetic (field 0 = separator String).
    // If the Path was produced by `FileSystem.getPath`, return that exact
    // FileSystem object (stored in P57_PATH_FS_FIELD) so identity checks
    // hold; otherwise fall back to a default FileSystem.
    r.register(
        path,
        "getFileSystem",
        "()Ljava/nio/file/FileSystem;",
        |ctx, args| {
            if let Ok(this) = obj_arg(args, 0) {
                if let Value::Object(Some(fs)) = ctx.get_field(this, P57_PATH_FS_FIELD) {
                    return Ok(Some(Value::Object(Some(fs))));
                }
                // Encoded virtual-FS paths produced during a walk carry no owning
                // FileSystem; reconstruct one so `path.getFileSystem().provider()
                // .getScheme()` reports jrt/jar (javac inspects this).
                let p = p57_read_path(ctx, this);
                if let Some((jh, _)) = jrtfs_decode(&p) {
                    let fs = p57_alloc_jrt_filesystem(ctx, &jh);
                    return Ok(Some(Value::Object(Some(fs))));
                }
                if let Some((jar, _)) = jarfs_decode(&p) {
                    let fs = p57_alloc_default_filesystem(ctx);
                    let jp = ctx.create_string(&jar);
                    ctx.set_field(fs, P57_FS_JAR_FIELD, Value::Object(Some(jp)));
                    return Ok(Some(Value::Object(Some(fs))));
                }
            }
            // No owning FileSystem recorded: this is a default-filesystem
            // path, so return THE default-FS singleton (identity checks
            // against FileSystems.getDefault() must hold).
            let fs = p57_default_filesystem_singleton(ctx);
            Ok(Some(Value::Object(Some(fs))))
        },
    );

    // --- Path.resolve(String) → Path ---
    r.register(
        path,
        "resolve",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other_ref = obj_arg(args, 1)?;
            let base = p57_read_path(ctx, this);
            let other = ctx.read_string(other_ref).unwrap_or_default();
            let resolved = p57_resolve_paths(&base, &other);
            let result = p57_alloc_path(ctx, &resolved);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // --- Path.resolve(Path) → Path ---
    r.register(
        path,
        "resolve",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let base = p57_read_path(ctx, this);
            let other_str = p57_read_path(ctx, other);
            let resolved = p57_resolve_paths(&base, &other_str);
            let result = p57_alloc_path(ctx, &resolved);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // --- Path.resolveSibling(String) → Path ---
    r.register(
        path,
        "resolveSibling",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other_ref = obj_arg(args, 1)?;
            let base = p57_read_path(ctx, this);
            let other = ctx.read_string(other_ref).unwrap_or_default();
            let parent = p57_parent_of(&base);
            let resolved = p57_resolve_paths(&parent, &other);
            let result = p57_alloc_path(ctx, &resolved);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // --- Path.resolveSibling(Path) → Path ---
    r.register(
        path,
        "resolveSibling",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let base = p57_read_path(ctx, this);
            let other_str = p57_read_path(ctx, other);
            let parent = p57_parent_of(&base);
            let resolved = p57_resolve_paths(&parent, &other_str);
            let result = p57_alloc_path(ctx, &resolved);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // --- Path.relativize(Path) → Path ---
    r.register(
        path,
        "relativize",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let base = p57_read_path(ctx, this);
            let target = p57_read_path(ctx, other);
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
                let result = p57_alloc_path(ctx, &rel);
                // Tag the result with the source's owning virtual FS so its
                // `toString()` renders with '/' (the zipfs/jrtfs separator), not
                // the host '\' — javac keys its package map on
                // `root.relativize(dir).toString()`.
                if let Value::Object(Some(fs)) = ctx.get_field(this, P57_PATH_FS_FIELD) {
                    ctx.set_field(result, P57_PATH_FS_FIELD, Value::Object(Some(fs)));
                }
                return Ok(Some(Value::Object(Some(result))));
            }
            // Host paths: compute the real relative path (with `..` backtracking)
            // off the shared root, not just a forward strip_prefix. Falls back to
            // `target` when the roots differ (can't be relativized).
            let relative = p57_relativize(&base, &target).unwrap_or_else(|| target.clone());
            let result = p57_alloc_path(ctx, &relative);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // --- Path.getParent() → Path (or null) ---
    r.register(path, "getParent", "()Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let parent = p57_parent_of(&p);
        if parent.is_empty() {
            Ok(Some(Value::Object(None)))
        } else {
            let result = p57_alloc_path(ctx, &parent);
            Ok(Some(Value::Object(Some(result))))
        }
    });

    // --- Path.getRoot() → Path (or null) ---
    r.register(path, "getRoot", "()Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        // keycloak-15: explicit Windows drive/UNC root parsing (the stored string
        // is '/'-canonical, so the old `[2]==b'\\'` check never matched a drive).
        match p57_parse_root(&p).0 {
            Some(root) => {
                let result = p57_alloc_path(ctx, &root);
                Ok(Some(Value::Object(Some(result))))
            }
            None => Ok(Some(Value::Object(None))),
        }
    });

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
    r.register(
        path,
        "getFileName",
        "()Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p_raw = p57_read_path(ctx, this);
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
                    let result = p57_alloc_path(ctx, &n);
                    Ok(Some(Value::Object(Some(result))))
                }
                None => {
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
                    let result = p57_alloc_path(ctx, "");
                    Ok(Some(Value::Object(Some(result))))
                }
            }
        },
    );

    // --- Path.toString() → String ---
    r.register(path, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let display = match vfs_decode(&p) {
            // jar-FS / jrt-FS Path.toString() shows the in-archive entry with
            // '/' (matches the JDK zipfs/jrtfs separator), regardless of host OS.
            Some((_, _, e)) => {
                if e.starts_with('/') {
                    e
                } else {
                    format!("/{e}")
                }
            }
            // A plain (non-encoded) relative path whose owning FileSystem is a
            // virtual (jar/jrt) FS renders with '/' — e.g. the result of
            // `jarRoot.relativize(dir)` ("org/h2/tools"), which javac turns into
            // a package name. Rendering the host '\' there would corrupt the key.
            None if path_owned_by_virtual_fs(ctx, this) => p.replace('\\', "/"),
            // Host-FS path: render the OS-native separator. CratonVM stores
            // paths with '/' internally, but HotSpot's WindowsPath.toString()
            // renders '\'; convert at this display boundary on Windows
            // (no-op on Unix). Matches `File.getPath()` below.
            None => file_normalise_path(&p),
        };
        let s = ctx.create_string(&display);
        Ok(Some(Value::Object(Some(s))))
    });

    // --- Path.equals(Object) → boolean ---
    r.register(path, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p1 = p57_read_path(ctx, this);
        match args.get(1) {
            Some(Value::Object(Some(other))) => {
                let p2 = p57_read_path(ctx, *other);
                Ok(Some(Value::Int(if p1 == p2 { 1 } else { 0 })))
            }
            _ => Ok(Some(Value::Int(0))),
        }
    });

    // --- Path.hashCode() → int ---
    r.register(path, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        // Simple hash matching Java's hashCode() contract
        let mut h: i32 = 0;
        for b in p.bytes() {
            h = h.wrapping_mul(31).wrapping_add(b as i32);
        }
        Ok(Some(Value::Int(h)))
    });

    // --- FileSystems.getDefault() → FileSystem ---
    let file_systems = "java/nio/file/FileSystems";
    r.register(
        file_systems,
        "getDefault",
        "()Ljava/nio/file/FileSystem;",
        |ctx, _args| {
            let fs = p57_default_filesystem_singleton(ctx);
            Ok(Some(Value::Object(Some(fs))))
        },
    );
    r.register(
        file_systems,
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;Ljava/lang/ClassLoader;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            let jar_path = obj_arg(args, 0)
                .ok()
                .map(|p| p57_read_path(ctx, p))
                .unwrap_or_default();
            let fs = p57_alloc_jar_filesystem(ctx, &jar_path);
            Ok(Some(Value::Object(Some(fs))))
        },
    );
    r.register(
        file_systems,
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            let jar_path = obj_arg(args, 0)
                .ok()
                .map(|p| p57_read_path(ctx, p))
                .unwrap_or_default();
            let fs = p57_alloc_jar_filesystem(ctx, &jar_path);
            Ok(Some(Value::Object(Some(fs))))
        },
    );
    r.register(
        file_systems,
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/lang/ClassLoader;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
            let jar_path = obj_arg(args, 0)
                .ok()
                .map(|p| p57_read_path(ctx, p))
                .unwrap_or_default();
            let fs = p57_alloc_jar_filesystem(ctx, &jar_path);
            Ok(Some(Value::Object(Some(fs))))
        },
    );

    // --- FileSystem methods ---
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
            if let Ok(this) = obj_arg(args, 0) {
                let is_virtual = matches!(
                    ctx.get_field(this, P57_FS_JAR_FIELD),
                    Value::Object(Some(_))
                ) || matches!(
                    ctx.get_field(this, P57_FS_JRT_FIELD),
                    Value::Object(Some(_))
                );
                if is_virtual {
                    let s = ctx.create_string("/");
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            let sep = if cfg!(windows) { "\\" } else { "/" };
            let s = ctx.create_string(sep);
            Ok(Some(Value::Object(Some(s))))
        },
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
            let jar = match ctx.get_field(this, P57_FS_JAR_FIELD) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let jrt = match ctx.get_field(this, P57_FS_JRT_FIELD) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let result = if !jar.is_empty() {
                p57_alloc_path(ctx, &jarfs_encode(&jar, &first))
            } else if !jrt.is_empty() {
                p57_alloc_path(ctx, &jrtfs_encode(&jrt, &first))
            } else {
                p57_alloc_path(ctx, &first)
            };
            // Record the FileSystem that produced this Path so
            // `Path.getFileSystem()` returns the *same* object — required by
            // identity checks like cassandra's `File(Path)` constructor
            // (`path.getFileSystem() == FileSystems.getDefault()`).
            ctx.set_field(result, P57_PATH_FS_FIELD, Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(result))))
        },
    );

    r.register(
        fs_class,
        "provider",
        "()Ljava/nio/file/spi/FileSystemProvider;",
        |ctx, args| {
            // Report the scheme matching this FileSystem so callers that check
            // `fs.provider().getScheme()` see "jrt"/"jar" for a mounted virtual
            // FS, "file" otherwise.
            let scheme_str = match obj_arg(args, 0) {
                Ok(this)
                    if matches!(
                        ctx.get_field(this, P57_FS_JRT_FIELD),
                        Value::Object(Some(_))
                    ) =>
                {
                    "jrt"
                }
                Ok(this)
                    if matches!(
                        ctx.get_field(this, P57_FS_JAR_FIELD),
                        Value::Object(Some(_))
                    ) =>
                {
                    "jar"
                }
                _ => "file",
            };
            let provider =
                alloc_concurrent_synthetic(ctx, "java/nio/file/spi/FileSystemProvider", 1);
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh provider (native stale-local family).
            let provider_pin = ctx.pin_native_root(provider);
            let scheme = ctx.create_string(scheme_str);
            let provider = ctx.read_native_pin(provider_pin, provider);
            ctx.set_field(provider, 0, Value::Object(Some(scheme)));
            ctx.unpin_native_roots(provider_pin);
            Ok(Some(Value::Object(Some(provider))))
        },
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
    });

    // Was unconditionally writable. A mounted runtime image (`jrt:`) is
    // read-only by definition, and this VM's jar-FS mount is read-only too
    // (`jarfs_*` has no in-place update path), so a caller that checked
    // `isReadOnly()` before attempting a write was told to go ahead and then
    // failed further downstream with an unrelated error.
    r.register(fs_class, "isReadOnly", "()Z", |ctx, args| {
        let virtual_fs = matches!(obj_arg(args, 0), Ok(this)
            if matches!(ctx.get_field(this, P57_FS_JAR_FIELD), Value::Object(Some(_)))
                || matches!(ctx.get_field(this, P57_FS_JRT_FIELD), Value::Object(Some(_))));
        Ok(Some(Value::Int(i32::from(virtual_fs))))
    });

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
        let virtual_fs = matches!(obj_arg(args, 0), Ok(this)
            if matches!(ctx.get_field(this, P57_FS_JAR_FIELD), Value::Object(Some(_)))
                || matches!(ctx.get_field(this, P57_FS_JRT_FIELD), Value::Object(Some(_))));
        if virtual_fs {
            return Ok(None);
        }
        Err(RuntimeError::UnsupportedOperationException {
            message: "The default file system cannot be closed".into(),
        }
        .into())
    });

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
            let is_virtual = matches!(obj_arg(args, 0), Ok(this)
                if matches!(ctx.get_field(this, P57_FS_JAR_FIELD), Value::Object(Some(_)))
                    || matches!(ctx.get_field(this, P57_FS_JRT_FIELD), Value::Object(Some(_))));
            if is_virtual {
                let s = alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptySet", 0);
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
            let keys: Vec<ObjectRef> = names.iter().map(|s| ctx.create_string(s)).collect();
            let s = crate::build_real_layout_string_hashset(ctx, &keys);
            Ok(Some(Value::Object(Some(s))))
        },
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
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 3);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(list, 0, Value::Int(0));
            ctx.set_field(list, 1, Value::Object(Some(arr)));
            ctx.set_field(list, 2, Value::Int(0));
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(list))))
        },
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
    // `springboot/filewatcher-watchservice-surface-FIXED-20260801.md`.

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
            let jar_field = this_pin.map(|(_, this)| ctx.get_field(this, P57_FS_JAR_FIELD));
            let jrt_field = this_pin.map(|(_, this)| ctx.get_field(this, P57_FS_JRT_FIELD));
            let root_path = match (jar_field, jrt_field) {
                (Some(Value::Object(Some(s))), _) => {
                    let jar = ctx.read_string(s).unwrap_or_default();
                    let rp = p57_alloc_path(ctx, &jarfs_encode(&jar, ""));
                    if let Some((h, orig)) = this_pin {
                        let this = ctx.read_native_pin(h, orig);
                        ctx.set_field(rp, P57_PATH_FS_FIELD, Value::Object(Some(this)));
                    }
                    rp
                }
                (_, Some(Value::Object(Some(s)))) => {
                    // Runtime image: root is the synthetic `/` whose only child is
                    // `/modules` (javac walks `/modules/<module>/...`).
                    let jh = ctx.read_string(s).unwrap_or_default();
                    let rp = p57_alloc_path(ctx, &jrtfs_encode(&jh, ""));
                    if let Some((h, orig)) = this_pin {
                        let this = ctx.read_native_pin(h, orig);
                        ctx.set_field(rp, P57_PATH_FS_FIELD, Value::Object(Some(this)));
                    }
                    rp
                }
                _ => {
                    let root = if cfg!(windows) { "C:\\" } else { "/" };
                    p57_alloc_path(ctx, root)
                }
            };
            let root_path_pin = ctx.pin_native_root(root_path);
            let arr = ctx.new_array(ArrayElementType::Reference, 1);
            let arr_pin = ctx.pin_native_root(arr);
            let root_path = ctx.read_native_pin(root_path_pin, root_path);
            ctx.set_array_element(arr, 0, Value::Object(Some(root_path)));
            // Real ArrayList field layout in real-JDK mode:
            //   [0]=AbstractList.modCount (int), [1]=elementData (Object[]), [2]=size (int).
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 3);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(list, 0, Value::Int(0));
            ctx.set_field(list, 1, Value::Object(Some(arr)));
            ctx.set_field(list, 2, Value::Int(1));
            ctx.unpin_native_roots(this_pin.map(|(h, _)| h).unwrap_or(root_path_pin));
            Ok(Some(Value::Object(Some(list))))
        },
    );

    // --- FileSystemProvider minimal methods ---
    let fsp = "java/nio/file/spi/FileSystemProvider";
    r.register(fsp, "getScheme", "()Ljava/lang/String;", |ctx, args| {
        // Read scheme from instance field 0 (populated by provider()/installedProviders()).
        // Falls back to "file" for legacy callers that allocated without a scheme slot.
        if let Some(this) = obj_arg(args, 0).ok() {
            if let Value::Object(Some(s)) = ctx.get_field(this, 0) {
                return Ok(Some(Value::Object(Some(s))));
            }
        }
        let s = ctx.create_string("file");
        Ok(Some(Value::Object(Some(s))))
    });

    // FileSystemProvider.isSameFile(Path, Path) — abstract on the base, so the
    // synthetic provider lacking it made `Files.isSameFile` dispatch to the
    // abstract method ("no Code attribute" AbstractMethodError). The default
    // provider's same-file check is path equality (real-path resolution for
    // symlinks omitted); approximate with `Path.equals`.
    r.register(
        fsp,
        "isSameFile",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let p1 = match obj_arg(args, 1) {
                Ok(p) => p,
                Err(_) => return Ok(Some(Value::Int(0))),
            };
            let p2 = match args.get(2) {
                Some(Value::Object(Some(p))) => *p,
                _ => return Ok(Some(Value::Int(0))),
            };
            let eq = matches!(
                ctx.invoke_virtual(p1, "equals", "(Ljava/lang/Object;)Z", &[Value::Object(Some(p2))]),
                Ok(Some(Value::Int(n))) if n != 0
            );
            Ok(Some(Value::Int(i32::from(eq))))
        },
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
            // arg 0 = provider (this); arg 1 = Path to the JAR to mount.
            // Mount the JAR's interior as a real jar-backed FileSystem so
            // `getPath`/`resolve`/`Files.*` resolve to archive entries rather
            // than non-existent host paths (smallrye ClassPathUtils relies on
            // walking the mounted jar to find config sources).
            let jar_path = obj_arg(args, 1)
                .ok()
                .map(|p| p57_read_path(ctx, p))
                .unwrap_or_default();
            let fs = p57_alloc_jar_filesystem(ctx, &jar_path);
            Ok(Some(Value::Object(Some(fs))))
        },
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
            let uri = obj_arg(args, 1)?;
            let text = p57_uri_full_text(ctx, uri);
            // A jrt: URI mounts the runtime image (HIB-CV-27 in-process javac).
            if text.starts_with("jrt:") {
                let jh = ctx.get_system_property("java.home").unwrap_or_default();
                let fs = p57_alloc_jrt_filesystem(ctx, &jh);
                return Ok(Some(Value::Object(Some(fs))));
            }
            if let Some((jar, _entry)) = p57_jar_uri_to_entry_path(&text) {
                // Mount file-backed jar/zip URIs even when the archive does not
                // exist yet. HotSpot's zipfs supports Map.of("create", "true");
                // callers then populate it through Files.createDirectories/copy.
                // CratonVM's jarfs writer below creates the archive lazily.
                let fs = p57_alloc_jar_filesystem(ctx, &jar);
                return Ok(Some(Value::Object(Some(fs))));
            }
            let fs = p57_alloc_default_filesystem(ctx);
            Ok(Some(Value::Object(Some(fs))))
        },
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
    r.register(
        fsp,
        "getFileAttributeView",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;",
        |ctx, args| {
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
            // docs/known-issues/h2-suite-bugs/bug-h2-files-setposixfilepermissions-unsupported.md.
            let is_posix = !cfg!(windows) && view_name.ends_with("PosixFileAttributeView");
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
            let view = alloc_concurrent_synthetic(ctx, vclass, 1);
            let path_obj = ctx.read_native_pin(path_pin, path_obj);
            ctx.set_field(view, 0, Value::Object(Some(path_obj)));
            ctx.unpin_native_roots(path_pin);
            Ok(Some(Value::Object(Some(view))))
        },
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
        },
    );
    r.register(
        "java/nio/file/attribute/BasicFileAttributeView",
        "name",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string("basic"))))),
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
            let dos =
                alloc_concurrent_synthetic(ctx, "java/nio/file/attribute/DosFileAttributes", 6);
            // Pin across each allocation below — a moving young GC there would
            // relocate the fresh attributes (native stale-local family).
            let dos_pin = ctx.pin_native_root(dos);
            let ct = filetime_alloc(ctx, creation);
            let dos_cur = ctx.read_native_pin(dos_pin, dos);
            ctx.set_field(dos_cur, 0, Value::Object(Some(ct)));
            let at = filetime_alloc(ctx, access);
            let dos_cur = ctx.read_native_pin(dos_pin, dos);
            ctx.set_field(dos_cur, 1, Value::Object(Some(at)));
            let mt = filetime_alloc(ctx, modified);
            let dos_cur = ctx.read_native_pin(dos_pin, dos);
            ctx.set_field(dos_cur, 2, Value::Object(Some(mt)));
            ctx.set_field(dos_cur, 3, Value::Int(i32::from(is_dir)));
            ctx.set_field(dos_cur, 4, Value::Long(size));
            let ps = ctx.create_string(&path);
            let dos_cur = ctx.read_native_pin(dos_pin, dos);
            ctx.set_field(dos_cur, 5, Value::Object(Some(ps)));
            ctx.unpin_native_roots(dos_pin);
            Ok(Some(Value::Object(Some(dos_cur))))
        },
    );
    r.register(
        "java/nio/file/attribute/DosFileAttributeView",
        "name",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string("dos"))))),
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
                set_file_attribute_times(&path, creation, access, modified)
                    .map_err(|error| p57_io_error(&error))?;
                Ok(None)
            },
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
        r.register(dos_view, "setReadOnly", "(Z)V", dos_view_set_read_only);
        r.register(dos_view, "setHidden", "(Z)V", dos_view_set_hidden);
        r.register(dos_view, "setSystem", "(Z)V", dos_view_set_system);
        r.register(dos_view, "setArchive", "(Z)V", dos_view_set_archive);
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
        },
    );
    r.register(
        "java/nio/file/attribute/PosixFileAttributeView",
        "name",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string("posix"))))),
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
        },
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
        },
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
        },
    );
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
            },
        );
        r.register(
            dfa,
            "lastAccessTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(ctx.get_field(this, 1)))
            },
        );
        r.register(
            dfa,
            "lastModifiedTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(ctx.get_field(this, 2)))
            },
        );
        r.register(dfa, "isDirectory", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        });
        r.register(dfa, "isRegularFile", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is_dir = matches!(ctx.get_field(this, 3), Value::Int(1));
            Ok(Some(Value::Int(if is_dir { 0 } else { 1 })))
        });
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
        });
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
        });
        r.register(dfa, "size", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 4)))
        });
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
        });
        r.register(dfa, "isReadOnly", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            let ro = !p.is_empty()
                && std::fs::metadata(&p)
                    .map(|m| m.permissions().readonly())
                    .unwrap_or(false);
            Ok(Some(Value::Int(i32::from(ro))))
        });
        r.register(dfa, "isHidden", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            Ok(Some(Value::Int(i32::from(dos_attr_flag(
                &p,
                DOS_ATTR_HIDDEN,
            )))))
        });
        r.register(dfa, "isArchive", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            Ok(Some(Value::Int(i32::from(dos_attr_flag(
                &p,
                DOS_ATTR_ARCHIVE,
            )))))
        });
        r.register(dfa, "isSystem", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = dos_attrs_path(ctx, this);
            Ok(Some(Value::Int(i32::from(dos_attr_flag(
                &p,
                DOS_ATTR_SYSTEM,
            )))))
        });
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
            let path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, path_obj);
            Ok(Some(Value::Object(Some(p57_alloc_file_store(ctx, &p)))))
        },
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
        let fs_store = "java/nio/file/FileStore";
        r.register(fs_store, "name", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(Value::Object(Some(ctx.create_string(&name)))))
        });
        r.register(fs_store, "type", "()Ljava/lang/String;", |ctx, _args| {
            let t = if cfg!(windows) { "NTFS" } else { "ext4" };
            Ok(Some(Value::Object(Some(ctx.create_string(t)))))
        });
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
            let this = obj_arg(args, 0)?;
            let path = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let ro = file_store_is_read_only(&path).unwrap_or(false);
            Ok(Some(Value::Int(i32::from(ro))))
        });
        // These three were `Long::MAX_VALUE` — "infinite disk". Any caller
        // doing capacity planning (ES's disk-threshold allocation decider,
        // Spring Boot's `DiskSpaceHealthIndicator`, an installer's free-space
        // pre-check) got a fabricated answer and could never report a full
        // disk. `file_disk_space_bytes` is the same real query the
        // `java.io.File` disk-space natives already use; field 0 holds the
        // store's mount root.
        r.register(fs_store, "getTotalSpace", "()J", |ctx, args| {
            Ok(Some(Value::Long(file_store_space(ctx, args, 0))))
        });
        r.register(fs_store, "getUsableSpace", "()J", |ctx, args| {
            Ok(Some(Value::Long(file_store_space(ctx, args, 2))))
        });
        r.register(fs_store, "getUnallocatedSpace", "()J", |ctx, args| {
            Ok(Some(Value::Long(file_store_space(ctx, args, 1))))
        });
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
            let this = obj_arg(args, 0)?;
            let path = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let block = file_store_block_size(&path).unwrap_or(4096);
            Ok(Some(Value::Long(block as i64)))
        });
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
                let supported = obj_arg(args, 1)
                    .ok()
                    .and_then(|m| crate::lang_class::mirror_class_name(ctx, m))
                    .as_deref()
                    .and_then(attribute_view_short_name)
                    .map(|n| supported_attribute_view_names().contains(&n))
                    .unwrap_or(false);
                Ok(Some(Value::Int(i32::from(supported))))
            },
        );
        r.register(
            fs_store,
            "supportsFileAttributeView",
            "(Ljava/lang/String;)Z",
            |ctx, args| {
                let name = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => return Ok(Some(Value::Int(0))),
                };
                let supported = supported_attribute_view_names().contains(&name.as_str());
                Ok(Some(Value::Int(i32::from(supported))))
            },
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
            |_ctx, args| {
                if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("FileStore.getFileStoreAttributeView: null view".into()),
                    }
                    .into());
                }
                Ok(Some(Value::Object(None)))
            },
        );
        r.register(
            fs_store,
            "getAttribute",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            |_ctx, _args| {
                Err(RuntimeError::UnsupportedOperationException {
                    message: "no such attribute".into(),
                }
                .into())
            },
        );
        r.register(fs_store, "toString", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let t = if cfg!(windows) { "NTFS" } else { "ext4" };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&format!("{name} ({t})")),
            ))))
        });
    }

    r.register(
        fsp,
        "installedProviders",
        "()Ljava/util/List;",
        |ctx, _args| {
            use cratonvm_types::ArrayElementType;
            let mk_provider = |ctx: &mut dyn NativeContext, scheme: &str| {
                let p = alloc_concurrent_synthetic(ctx, "java/nio/file/spi/FileSystemProvider", 1);
                // Pin across the create_string below — a moving young GC there
                // would relocate the fresh provider (native stale-local family).
                let p_pin = ctx.pin_native_root(p);
                let s = ctx.create_string(scheme);
                let p = ctx.read_native_pin(p_pin, p);
                ctx.set_field(p, 0, Value::Object(Some(s)));
                ctx.unpin_native_roots(p_pin);
                p
            };
            // "jrt" lets `FileSystems.getFileSystem(URI.create("jrt:/"))` resolve
            // (the real-JDK static iterates installedProviders by scheme) so the
            // in-process compiler can read platform classes from the runtime
            // image (HIB-CV-27).
            let file_p = mk_provider(ctx, "file");
            let file_pin = ctx.pin_native_root(file_p);
            let jar_p = mk_provider(ctx, "jar");
            let jar_pin = ctx.pin_native_root(jar_p);
            let jrt_p = mk_provider(ctx, "jrt");
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
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 3);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(list, 0, Value::Int(0));
            ctx.set_field(list, 1, Value::Object(Some(arr)));
            ctx.set_field(list, 2, Value::Int(3));
            ctx.unpin_native_roots(file_pin);
            Ok(Some(Value::Object(Some(list))))
        },
    );

    r.register(
        fsp,
        "getFileSystem",
        "(Ljava/net/URI;)Ljava/nio/file/FileSystem;",
        |ctx, args| {
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
                let fs = p57_alloc_jrt_filesystem(ctx, &jh);
                return Ok(Some(Value::Object(Some(fs))));
            }
            let fs = p57_alloc_default_filesystem(ctx);
            Ok(Some(Value::Object(Some(fs))))
        },
    );

    r.register(
        fsp,
        "getPath",
        "(Ljava/net/URI;)Ljava/nio/file/Path;",
        |ctx, args| {
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
                let fs = p57_alloc_jar_filesystem(ctx, &jar);
                let result = p57_alloc_path(ctx, &jarfs_encode(&jar, &entry));
                ctx.set_field(result, P57_PATH_FS_FIELD, Value::Object(Some(fs)));
                return Ok(Some(Value::Object(Some(result))));
            }
            // `URI.getPath()` first -- the DECODED path component (see the
            // matching note in the `Path.of(URI)` native above); the field
            // probes below all yield the RAW, still percent-encoded form.
            if let Ok(Some(Value::Object(Some(s)))) =
                ctx.invoke_virtual(uri, "getPath", "()Ljava/lang/String;", &[])
            {
                if let Some(decoded) = ctx.read_string(s) {
                    if !decoded.is_empty() {
                        let os_path = p57_to_os_path(&decoded);
                        let result = p57_alloc_path(ctx, &os_path);
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
            let result = p57_alloc_path(ctx, &os_path);
            Ok(Some(Value::Object(Some(result))))
        },
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
    // real-JDK arm (vm_init.rs:1788 and :2273). Everything registered in this
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
        |_ctx, _args| Ok(Some(Value::Object(None))),
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
                        .class_name_of_id(ctx.class_id_of_object(value))
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
        },
    );

    // Keycloak's legacy Config.Scope consumer calls this concrete one-argument
    // method. Route it through the inherited two-argument lookup so config
    // resolution follows the same path as the JDK implementation.
    r.register(
        "org/keycloak/quarkus/runtime/configuration/MicroProfileConfigProvider$MicroProfileScope",
        "get",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let this_pin = ctx.pin_native_root(this);
            let name_pin = ctx.pin_native_root(name);
            let this_cur = ctx.read_native_pin(this_pin, this);
            let name_cur = ctx.read_native_pin(name_pin, name);
            let result = ctx.invoke_virtual(
                this_cur,
                "get",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[Value::Object(Some(name_cur)), Value::Object(None)],
            );
            ctx.unpin_native_roots(this_pin);
            ctx.unpin_native_roots(name_pin);
            result
        },
    );

    // --- smallrye-config KeyStoreConfigSourceFactory.getConfigSources ---
    // CratonVM divergence: In real Quarkus, the @ConfigMapping interface
    // io.smallrye.config.source.keystore.KeyStoreConfig is auto-registered with
    // the SmallRyeConfig instance via build-time discovery/reflection.  Under
    // CratonVM that registration path is not active, so the factory's call to
    // SmallRyeConfig.getConfigMapping(KeyStoreConfig.class) throws
    // NoSuchElementException("SRCFG00027: Could not find a mapping for
    // io.smallrye.config.source.keystore.KeyStoreConfig"), aborting Keycloak
    // 26 boot during Configuration.getConfig().  Keycloak's default config
    // does not use keystore-backed config sources, so returning an empty
    // Iterable here is semantically equivalent to having no configured
    // keystores.  Short-circuit at getConfigSources so we never hit the
    // mapping lookup in getKeyStoreConfig.
    #[cfg(any())]
    r.register(
        "io/smallrye/config/source/keystore/KeyStoreConfigSourceFactory",
        "getConfigSources",
        "(Lio/smallrye/config/ConfigSourceContext;)Ljava/lang/Iterable;",
        |ctx, _args| {
            use cratonvm_types::ArrayElementType;
            let arr = ctx.new_array(ArrayElementType::Reference, 0);
            // Pin across the list alloc below — a moving young GC there would
            // relocate the fresh array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 3);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(list, 0, Value::Int(0));
            ctx.set_field(list, 1, Value::Object(Some(arr)));
            ctx.set_field(list, 2, Value::Int(0));
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(list))))
        },
    );

    // --- SmallRyeConfig.getConfigMapping(Class, String) — Keycloak Gap 6 ---
    // Quarkus recorder steps (e.g. VirtualThreads) and `SharedConfig.<clinit>`
    // call `getConfigMapping(<@ConfigMapping iface>.class, prefix)` to get the
    // config-mapping impl. The real method reads the config's `mappings`
    // registry, which CratonVM never populates → the old "Round 87" shim
    // allocated a synthetic of the INTERFACE (no method bodies) → callers like
    // `VirtualThreadsConfig.enabled()` threw `AbstractMethodError`.
    //
    // The native now LAZILY REGISTERS the requested mapping with the live
    // `SmallRyeConfig` via the real `ConfigMappings.registerConfigMappings` and
    // returns the genuine, config-backed `<iface>$$CMImpl`. See
    // `native_smallrye_get_config_mapping` above for the full rationale.
    r.register(
        "io/smallrye/config/SmallRyeConfig",
        "getConfigMapping",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Object;",
        native_smallrye_get_config_mapping,
    );

    // The single-arg form `getConfigMapping(Class)` delegates to the two-arg
    // form on real SmallRyeConfig, but when the JIT/interp doesn't re-enter
    // the bytecode path (e.g. direct invokevirtual without inline cache), we
    // mirror the same logic here so both arms are covered. This used to
    // allocate a bare synthetic of the mapping INTERFACE (no method bodies),
    // which threw AbstractMethodError on the first call (e.g.
    // `TestConfig.classOrderer()` during Quarkus JUnit discovery) — the same
    // bug class the 2-arg form's Gap-6 fix already solved. Derive the real
    // prefix from `@ConfigMapping` and reuse that fixed path.
    r.register(
        "io/smallrye/config/SmallRyeConfig",
        "getConfigMapping",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            let class_mirror = match args.get(1) {
                Some(Value::Object(Some(c))) => *c,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Pin across the annotation-prefix lookup below — a moving young
            // GC there would relocate them (native stale-local family).
            let this_val = args[0];
            let this_pin = pinned_object_value(ctx, this_val);
            let mirror_pin = ctx.pin_native_root(class_mirror);
            let prefix = config_mapping_prefix(ctx, class_mirror);
            let this_val = read_pinned_object_value(ctx, this_pin, this_val);
            let class_mirror = ctx.read_native_pin(mirror_pin, class_mirror);
            if let Some((h, _)) = this_pin {
                ctx.unpin_native_roots(h);
            } else {
                ctx.unpin_native_roots(mirror_pin);
            }
            native_smallrye_get_config_mapping(
                ctx,
                &[this_val, Value::Object(Some(class_mirror)), prefix],
            )
        },
    );

    // --- Quarkus LoggingSetupRecorder.handleFailedStart ---
    // Keycloak's test framework registers a SmallRye config that already has
    // Quarkus' Charset/MemorySize converters. The recorder builds a transient
    // logging config from it; under CratonVM that transient builder needs the
    // discovered Quarkus converters made explicit before mapping validation.
    r.register(
        "io/quarkus/runtime/logging/LoggingSetupRecorder",
        "handleFailedStart",
        "()V",
        |ctx, _args| native_quarkus_logging_handle_failed_start(ctx, None),
    );
    r.register(
        "io/quarkus/runtime/logging/LoggingSetupRecorder",
        "handleFailedStart",
        "(Lio/quarkus/runtime/RuntimeValue;)V",
        |ctx, args| {
            let supplier = match args.first() {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            native_quarkus_logging_handle_failed_start(ctx, supplier)
        },
    );

    // --- picocli CommandLine$Help$Ansi$Text.concat(Text) ---
    // Keycloak's PicocliTest builds large help/usage strings through picocli's
    // immutable-ish `Ansi.Text` API. The Java `concat(Text)` path clones the
    // receiver, slices two `StringBuilder`s, allocates a fresh `ArrayList`, and
    // re-materializes every `StyledSection` through a tiny constructor. Under
    // CratonVM this hot path dominated the class enough to look like a hang;
    // HotSpot JIT finishes the same class quickly. Keep the optimization
    // app-scoped: preserve Picocli's object shape, but perform the clone/slice
    // and section reindexing in one native pass.
    #[derive(Clone)]
    struct PicocliStyledSectionData {
        start_index: i32,
        length: i32,
        start_styles: String,
        end_styles: String,
    }

    fn picocli_i32_field(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> i32 {
        match ctx.get_field_by_name(obj, name) {
            Value::Int(v) => v,
            _ => 0,
        }
    }

    fn picocli_obj_field(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> Option<ObjectRef> {
        match ctx.get_field_by_name(obj, name) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        }
    }

    fn picocli_string_field(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> String {
        picocli_obj_field(ctx, obj, name)
            .and_then(|s| ctx.read_string(s))
            .unwrap_or_default()
    }

    fn picocli_utf16_len(s: &str) -> i32 {
        s.encode_utf16().count().min(i32::MAX as usize) as i32
    }

    fn picocli_utf16_slice(s: &str, from: i32, len: i32) -> String {
        let units: Vec<u16> = s.encode_utf16().collect();
        let total = units.len();
        let start = (from.max(0) as usize).min(total);
        let wanted = len.max(0) as usize;
        let end = start.saturating_add(wanted).min(total);
        String::from_utf16_lossy(&units[start..end])
    }

    fn picocli_list_size(ctx: &mut dyn NativeContext, list: ObjectRef) -> usize {
        match ctx.get_field_by_name(list, "size") {
            Value::Int(n) if n > 0 => n as usize,
            Value::Int(_) => 0,
            _ => match ctx.invoke_virtual(list, "size", "()I", &[]) {
                Ok(Some(Value::Int(n))) if n > 0 => n as usize,
                _ => 0,
            },
        }
    }

    fn picocli_list_get(
        ctx: &mut dyn NativeContext,
        list: ObjectRef,
        index: usize,
    ) -> Option<ObjectRef> {
        if let Value::Object(Some(data)) = ctx.get_field_by_name(list, "elementData") {
            if index < ctx.array_length(data) {
                if let Value::Object(Some(o)) = ctx.get_array_element(data, index) {
                    return Some(o);
                }
            }
        }
        match ctx.invoke_virtual(
            list,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Int(index.min(i32::MAX as usize) as i32)],
        ) {
            Ok(Some(Value::Object(Some(o)))) => Some(o),
            _ => None,
        }
    }

    fn picocli_collect_sections(
        ctx: &mut dyn NativeContext,
        list: Option<ObjectRef>,
        delta: i32,
    ) -> Vec<PicocliStyledSectionData> {
        let Some(list) = list else {
            return Vec::new();
        };
        let count = picocli_list_size(ctx, list).min(100_000);
        let mut out = Vec::with_capacity(count);
        for idx in 0..count {
            let Some(section) = picocli_list_get(ctx, list, idx) else {
                continue;
            };
            out.push(PicocliStyledSectionData {
                start_index: picocli_i32_field(ctx, section, "startIndex").saturating_add(delta),
                length: picocli_i32_field(ctx, section, "length"),
                start_styles: picocli_string_field(ctx, section, "startStyles"),
                end_styles: picocli_string_field(ctx, section, "endStyles"),
            });
        }
        out
    }

    fn picocli_text_plain_slice(ctx: &mut dyn NativeContext, text: ObjectRef) -> String {
        let full = match picocli_obj_field(ctx, text, "plain") {
            Some(plain) => crate::lang_string::invoke_to_string(ctx, plain).unwrap_or_default(),
            None => String::new(),
        };
        let from = picocli_i32_field(ctx, text, "from");
        let len = picocli_i32_field(ctx, text, "length");
        picocli_utf16_slice(&full, from, len)
    }

    fn picocli_new_styled_section_value(
        ctx: &mut dyn NativeContext,
        start_index: i32,
        length: i32,
        start_styles: &str,
        end_styles: &str,
    ) -> MethodCallResult {
        let obj = match ctx.new_object("picocli/CommandLine$Help$Ansi$StyledSection")? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let pin = ctx.pin_native_root(obj);
        let cur = ctx.read_native_pin(pin, obj);
        ctx.set_field_by_name(cur, "startIndex", Value::Int(start_index));
        ctx.set_field_by_name(cur, "length", Value::Int(length));

        let start_obj = ctx.create_string(start_styles);
        let cur = ctx.read_native_pin(pin, obj);
        ctx.set_field_by_name(cur, "startStyles", Value::Object(Some(start_obj)));

        let end_obj = ctx.create_string(end_styles);
        let cur = ctx.read_native_pin(pin, obj);
        ctx.set_field_by_name(cur, "endStyles", Value::Object(Some(end_obj)));
        let cur = ctx.read_native_pin(pin, obj);
        ctx.unpin_native_roots(pin);
        Ok(Some(Value::Object(Some(cur))))
    }

    fn native_picocli_styled_section_init(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let start = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let len = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        ctx.set_field_by_name(this, "startIndex", Value::Int(start));
        ctx.set_field_by_name(this, "length", Value::Int(len));
        ctx.set_field_by_name(
            this,
            "startStyles",
            args.get(3).copied().unwrap_or(Value::Object(None)),
        );
        ctx.set_field_by_name(
            this,
            "endStyles",
            args.get(4).copied().unwrap_or(Value::Object(None)),
        );
        Ok(None)
    }

    fn native_picocli_styled_section_with_start_index(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let new_start = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let len = picocli_i32_field(ctx, this, "length");
        let start_styles = picocli_string_field(ctx, this, "startStyles");
        let end_styles = picocli_string_field(ctx, this, "endStyles");
        picocli_new_styled_section_value(ctx, new_start, len, &start_styles, &end_styles)
    }

    fn native_picocli_text_concat_text(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(Some(this)))),
        };

        let root_pin = ctx.pin_native_root(this);
        let other_pin = ctx.pin_native_root(other);

        let left = picocli_text_plain_slice(ctx, this);
        let right = picocli_text_plain_slice(ctx, other);
        let left_len = picocli_utf16_len(&left);
        let self_from = picocli_i32_field(ctx, this, "from");
        let other_from = picocli_i32_field(ctx, other, "from");
        let this_sections = picocli_obj_field(ctx, this, "sections");
        let other_sections = picocli_obj_field(ctx, other, "sections");
        let mut sections = picocli_collect_sections(ctx, this_sections, self_from.saturating_neg());
        sections.extend(picocli_collect_sections(
            ctx,
            other_sections,
            left_len.saturating_sub(other_from),
        ));
        ctx.unpin_native_roots(other_pin);

        let combined = format!("{left}{right}");
        let combined_len = picocli_utf16_len(&combined);
        let combined_string = ctx.create_string(&combined);
        let string_pin = ctx.pin_native_root(combined_string);
        let combined_string = ctx.read_native_pin(string_pin, combined_string);
        let plain = match ctx.new_object_initialized(
            "java/lang/StringBuilder",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(combined_string))],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(string_pin);
                ctx.unpin_native_roots(root_pin);
                return Ok(Some(Value::Object(None)));
            }
        };
        ctx.unpin_native_roots(string_pin);

        let section_list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(root_pin);
                return Ok(Some(Value::Object(None)));
            }
        };
        let list_pin = ctx.pin_native_root(section_list);
        for section in sections {
            let section_val = picocli_new_styled_section_value(
                ctx,
                section.start_index,
                section.length,
                &section.start_styles,
                &section.end_styles,
            )?;
            if let Some(Value::Object(Some(section_obj))) = section_val {
                let list = ctx.read_native_pin(list_pin, section_list);
                let _ = ctx.invoke_virtual(
                    list,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(section_obj))],
                );
            }
        }
        let section_list = ctx.read_native_pin(list_pin, section_list);
        ctx.unpin_native_roots(list_pin);

        let plain_pin = ctx.pin_native_root(plain);
        let section_list_pin = ctx.pin_native_root(section_list);
        let result = match ctx.new_object("picocli/CommandLine$Help$Ansi$Text")? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(plain_pin);
                ctx.unpin_native_roots(section_list_pin);
                ctx.unpin_native_roots(root_pin);
                return Ok(Some(Value::Object(None)));
            }
        };
        let plain = ctx.read_native_pin(plain_pin, plain);
        let section_list = ctx.read_native_pin(section_list_pin, section_list);
        ctx.unpin_native_roots(plain_pin);

        let this = ctx.read_native_pin(root_pin, this);
        ctx.set_field_by_name(result, "this$0", ctx.get_field_by_name(this, "this$0"));
        ctx.set_field_by_name(
            result,
            "maxLength",
            ctx.get_field_by_name(this, "maxLength"),
        );
        ctx.set_field_by_name(result, "from", Value::Int(0));
        ctx.set_field_by_name(result, "length", Value::Int(combined_len));
        ctx.set_field_by_name(result, "plain", Value::Object(Some(plain)));
        ctx.set_field_by_name(result, "sections", Value::Object(Some(section_list)));
        ctx.unpin_native_roots(section_list_pin);
        ctx.set_field_by_name(
            result,
            "colorScheme",
            ctx.get_field_by_name(this, "colorScheme"),
        );
        ctx.unpin_native_roots(root_pin);
        Ok(Some(Value::Object(Some(result))))
    }

    fn picocli_is_code_point_cjk(cp: i32) -> bool {
        cp == 0x00B1
            || (0x3040..=0x309F).contains(&cp)
            || (0x30A0..=0x30FF).contains(&cp)
            || (0x31F0..=0x31FF).contains(&cp)
            || (0x3130..=0x318F).contains(&cp)
            || (0x1100..=0x11FF).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp)
            || (0x4E00..=0x9FFF).contains(&cp)
            || (0x3400..=0x4DBF).contains(&cp)
            || (0x20000..=0x2A6DF).contains(&cp)
            || (0xFE30..=0xFE4F).contains(&cp)
            || (0xF900..=0xFAFF).contains(&cp)
            || (0x2E80..=0x2EFF).contains(&cp)
            || (0x3000..=0x303F).contains(&cp)
            || (0x3200..=0x32FF).contains(&cp)
            || (0xFF00..=0xFF60).contains(&cp)
    }

    fn picocli_cjk_adjusted_len(s: &str) -> i32 {
        let mut width: i32 = 0;
        for ch in s.chars() {
            width = width.saturating_add(if picocli_is_code_point_cjk(ch as i32) {
                2
            } else {
                1
            });
        }
        width
    }

    fn picocli_text_plain_slice_with_len(
        ctx: &mut dyn NativeContext,
        text: ObjectRef,
        len: i32,
    ) -> String {
        let full = match picocli_obj_field(ctx, text, "plain") {
            Some(plain) => crate::lang_string::invoke_to_string(ctx, plain).unwrap_or_default(),
            None => String::new(),
        };
        let from = picocli_i32_field(ctx, text, "from");
        picocli_utf16_slice(&full, from, len)
    }

    fn native_picocli_is_code_point_cjk(
        _ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let cp = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        Ok(Some(Value::Int(if picocli_is_code_point_cjk(cp) {
            1
        } else {
            0
        })))
    }

    fn native_picocli_text_get_cjk_adjusted_length(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => picocli_i32_field(ctx, this, "length"),
        };
        let text = picocli_text_plain_slice_with_len(ctx, this, len);
        Ok(Some(Value::Int(picocli_cjk_adjusted_len(&text))))
    }

    r.register(
        "picocli/CommandLine$Help$Ansi$StyledSection",
        "<init>",
        "(IILjava/lang/String;Ljava/lang/String;)V",
        native_picocli_styled_section_init,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$StyledSection",
        "withStartIndex",
        "(I)Lpicocli/CommandLine$Help$Ansi$StyledSection;",
        native_picocli_styled_section_with_start_index,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$Text",
        "concat",
        "(Lpicocli/CommandLine$Help$Ansi$Text;)Lpicocli/CommandLine$Help$Ansi$Text;",
        native_picocli_text_concat_text,
    );

    r.register(
        "picocli/CommandLine$Model$UsageMessageSpec",
        "isCodePointCJK",
        "(I)Z",
        native_picocli_is_code_point_cjk,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$Text",
        "getCJKAdjustedLength",
        "()I",
        native_picocli_text_get_cjk_adjusted_length,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$Text",
        "getCJKAdjustedLength",
        "(II)I",
        native_picocli_text_get_cjk_adjusted_length,
    );

    // --- Keycloak PropertyMappers$WildcardMappersConfig.get(String) ---
    // `PicocliTest` repeatedly walks SmallRye config property names while
    // sanitizing command mappers. The Java implementation routes every kc.* /
    // quarkus.* key through `wildcardMappers.stream().filter(...).toList()`;
    // under CratonVM that stream/lambda path can spin at the predicate. Mirror
    // the simple Keycloak logic directly over the backing set and keep returning
    // the original mapper objects.
    fn keycloak_wildcard_value_valid(value: &str) -> bool {
        !value.is_empty()
            && value.chars().all(|ch| {
                ch.is_ascii_alphanumeric() || matches!(ch, '[' | ']' | '$' | '-' | '.' | '_')
            })
    }

    fn keycloak_wildcard_mapper_matches(
        ctx: &dyn NativeContext,
        mapper: ObjectRef,
        key: &str,
    ) -> bool {
        let from_prefix = picocli_string_field(ctx, mapper, "fromPrefix");
        if !from_prefix.is_empty() && key.starts_with(&from_prefix) {
            return keycloak_wildcard_value_valid(&key[from_prefix.len()..]);
        }

        let to_prefix = picocli_string_field(ctx, mapper, "toPrefix");
        let to_suffix = picocli_string_field(ctx, mapper, "toSuffix");
        if to_prefix.is_empty()
            || !key.starts_with(&to_prefix)
            || !key.ends_with(&to_suffix)
            || key.len() < to_prefix.len().saturating_add(to_suffix.len())
        {
            return false;
        }
        let end = key.len().saturating_sub(to_suffix.len());
        keycloak_wildcard_value_valid(&key[to_prefix.len()..end])
    }

    fn keycloak_empty_array_list(ctx: &mut dyn NativeContext) -> MethodCallResult {
        match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(list))) => Ok(Some(Value::Object(Some(list)))),
            _ => Ok(Some(Value::Object(None))),
        }
    }

    fn native_keycloak_wildcard_mappers_get(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return keycloak_empty_array_list(ctx),
        };
        let key = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };

        // Pin across the ArrayList ctor / iterator invokes below — a moving
        // young GC there would relocate `this`/`list` (native stale-local
        // family).
        let this_pin = ctx.pin_native_root(this);
        let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(list))) => list,
            _ => {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(None)));
            }
        };
        let list_pin = ctx.pin_native_root(list);
        let this = ctx.read_native_pin(this_pin, this);
        if !(key.starts_with("kc.") || key.starts_with("quarkus.")) {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(list))));
        }

        let Some(set) = picocli_obj_field(ctx, this, "wildcardMappers") else {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(list))));
        };
        let iterator = match ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[])? {
            Some(Value::Object(Some(iterator))) => iterator,
            _ => {
                let list = ctx.read_native_pin(list_pin, list);
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(list))));
            }
        };

        let iterator_pin = ctx.pin_native_root(iterator);
        for _ in 0..10_000 {
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let has_next = match ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])? {
                Some(Value::Int(v)) => v != 0,
                _ => false,
            };
            if !has_next {
                break;
            }

            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let mapper = match ctx.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(Some(mapper))) => mapper,
                _ => continue,
            };
            if keycloak_wildcard_mapper_matches(ctx, mapper, &key) {
                let mapper_pin = ctx.pin_native_root(mapper);
                let mapper = ctx.read_native_pin(mapper_pin, mapper);
                let list = ctx.read_native_pin(list_pin, list);
                let _ = ctx.invoke_virtual(
                    list,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(mapper))],
                )?;
                ctx.unpin_native_roots(mapper_pin);
            }
        }

        let list = ctx.read_native_pin(list_pin, list);
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Object(Some(list))))
    }

    fn keycloak_optional_string(ctx: &mut dyn NativeContext, value: Option<String>) -> Value {
        let optional = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
        // Pin across the create_string below — a moving young GC there would
        // relocate the fresh Optional (native stale-local family).
        let optional_pin = ctx.pin_native_root(optional);
        let optional_value = value
            .map(|s| Value::Object(Some(ctx.create_string(&s))))
            .unwrap_or(Value::Object(None));
        let optional = ctx.read_native_pin(optional_pin, optional);
        ctx.set_field(optional, 0, optional_value);
        ctx.unpin_native_roots(optional_pin);
        Value::Object(Some(optional))
    }

    fn keycloak_is_not_blank(value: &str) -> bool {
        value.chars().any(|ch| !ch.is_whitespace())
    }

    fn keycloak_prefixed_optional_field(
        ctx: &mut dyn NativeContext,
        obj: ObjectRef,
        field_name: &str,
        prefix: &str,
    ) -> MethodCallResult {
        let value = match ctx.get_field_by_name(obj, field_name) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let result = if keycloak_is_not_blank(&value) {
            Some(format!("{prefix}{value}"))
        } else {
            None
        };
        Ok(Some(keycloak_optional_string(ctx, result)))
    }

    fn native_keycloak_property_mapper_get_enabled_when(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(keycloak_optional_string(ctx, None))),
        };
        keycloak_prefixed_optional_field(ctx, this, "enabledWhen", "Available only when ")
    }

    fn native_keycloak_property_mapper_get_required_when(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(keycloak_optional_string(ctx, None))),
        };
        keycloak_prefixed_optional_field(ctx, this, "requiredWhen", "Required when ")
    }

    fn native_keycloak_wildcard_is_valid_value(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let value = match args.first() {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(Some(Value::Int(if keycloak_wildcard_value_valid(&value) {
            1
        } else {
            0
        })))
    }

    fn native_keycloak_wildcard_extract_value(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(keycloak_optional_string(ctx, None))),
        };
        let key = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let from_prefix = picocli_string_field(ctx, this, "fromPrefix");
        let candidate = if !from_prefix.is_empty() && key.starts_with(&from_prefix) {
            Some(key[from_prefix.len()..].to_string())
        } else {
            let to_prefix = picocli_string_field(ctx, this, "toPrefix");
            let to_suffix = picocli_string_field(ctx, this, "toSuffix");
            if !to_prefix.is_empty()
                && key.starts_with(&to_prefix)
                && key.ends_with(&to_suffix)
                && key.len() >= to_prefix.len().saturating_add(to_suffix.len())
            {
                let end = key.len().saturating_sub(to_suffix.len());
                Some(key[to_prefix.len()..end].to_string())
            } else {
                None
            }
        }
        .filter(|s| keycloak_wildcard_value_valid(s));
        Ok(Some(keycloak_optional_string(ctx, candidate)))
    }

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/PropertyMappers$WildcardMappersConfig",
        "get",
        "(Ljava/lang/String;)Ljava/util/List;",
        native_keycloak_wildcard_mappers_get,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/WildcardPropertyMapper",
        "isValidWildcardValue",
        "(Ljava/lang/String;)Z",
        native_keycloak_wildcard_is_valid_value,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/WildcardPropertyMapper",
        "extractWildcardValue",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        native_keycloak_wildcard_extract_value,
    );

    // --- SmallRyeConfigSources.getValue / MapBackedConfigValueConfigSource.getConfigValue ---
    // These methods are tiny dispatch loops in SmallRye config, but Picocli's
    // validation path calls them enough that CratonVM can spend minutes in the
    // interpreted interface chain. Keep the semantics intact: walk the exact
    // `configSources` list, return the first non-null ConfigValue, then delegate
    // to the interceptor context.
    fn native_smallrye_map_backed_get_config_value(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let Some(properties) = picocli_obj_field(ctx, this, "properties") else {
            return Ok(Some(Value::Object(None)));
        };
        ctx.invoke_virtual(
            properties,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[key],
        )
    }

    fn native_smallrye_config_sources_get_value(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let context = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let key = args.get(2).copied().unwrap_or(Value::Object(None));
        let Some(config_sources) = picocli_obj_field(ctx, this, "configSources") else {
            return ctx.invoke_virtual(
                context,
                "proceed",
                "(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
                &[key],
            );
        };

        let count = picocli_list_size(ctx, config_sources).min(100_000);
        for idx in 0..count {
            let Some(source) = picocli_list_get(ctx, config_sources, idx) else {
                continue;
            };
            let value = ctx.invoke_virtual(
                source,
                "getConfigValue",
                "(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
                &[key],
            )?;
            if matches!(value, Some(Value::Object(Some(_)))) {
                return Ok(value);
            }
        }

        ctx.invoke_virtual(
            context,
            "proceed",
            "(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
            &[key],
        )
    }

    r.register(
        "io/smallrye/config/MapBackedConfigValueConfigSource",
        "getConfigValue",
        "(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
        native_smallrye_map_backed_get_config_value,
    );
    r.register(
        "io/smallrye/config/SmallRyeConfigSources",
        "getValue",
        "(Lio/smallrye/config/ConfigSourceInterceptorContext;Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
        native_smallrye_config_sources_get_value,
    );

    // --- SmallRye simple property helpers ---
    // These preserve SmallRye's object shape while avoiding repeated Java
    // interpreter trips through trivial constructors/accessors on Picocli's
    // validation path. The relaxed-name matching bytecode still runs normally.
    fn smallrye_property_name_hash(name: &str) -> i32 {
        let mut hash = 0i32;
        let mut in_quote = false;
        for ch in name.encode_utf16() {
            if in_quote {
                if ch == b'"' as u16 {
                    in_quote = false;
                }
            } else if ch == b'"' as u16 {
                in_quote = true;
            } else if ch == b'[' as u16 || ch == b']' as u16 {
                hash = hash.wrapping_mul(31).wrapping_add(ch as i32);
            }
        }
        hash
    }

    fn native_smallrye_property_name_init(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let name_value = args.get(1).copied().unwrap_or(Value::Object(None));
        let hash = match name_value {
            Value::Object(Some(s)) => ctx
                .read_string(s)
                .map(|name| smallrye_property_name_hash(&name))
                .unwrap_or(0),
            _ => 0,
        };
        ctx.set_field_by_name(this, "name", name_value);
        ctx.set_field_by_name(this, "hashCode", Value::Int(hash));
        Ok(None)
    }

    fn native_smallrye_property_name_hash_code(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "hashCode")))
    }

    fn native_smallrye_property_name_string_field(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    }

    fn native_smallrye_sysprop_get_system_property(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        key_index: usize,
    ) -> MethodCallResult {
        let key = match args.get(key_index) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(match ctx.get_system_property(&key) {
            Some(value) => Value::Object(Some(ctx.create_string(&value))),
            None => Value::Object(None),
        }))
    }

    r.register(
        "io/smallrye/config/PropertyName",
        "<init>",
        "(Ljava/lang/String;)V",
        native_smallrye_property_name_init,
    );
    r.register(
        "io/smallrye/config/PropertyName",
        "hashCode",
        "()I",
        native_smallrye_property_name_hash_code,
    );
    r.register(
        "io/smallrye/config/PropertyName",
        "getName",
        "()Ljava/lang/String;",
        native_smallrye_property_name_string_field,
    );
    r.register(
        "io/smallrye/config/PropertyName",
        "toString",
        "()Ljava/lang/String;",
        native_smallrye_property_name_string_field,
    );
    r.register(
        "io/smallrye/config/SysPropConfigSource",
        "getValue",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| native_smallrye_sysprop_get_system_property(ctx, args, 1),
    );
    r.register(
        "io/smallrye/config/SysPropConfigSource",
        "getSystemProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| native_smallrye_sysprop_get_system_property(ctx, args, 0),
    );

    fn native_keycloak_transform_datasource_to(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input_obj = match args.first() {
            Some(Value::Object(Some(s))) => *s,
            _ => return Ok(Some(Value::Object(None))),
        };
        let input = ctx.read_string(input_obj).unwrap_or_default();
        if input.trim().is_empty() {
            return Ok(Some(Value::Object(None)));
        }
        let output = if let Some(rest) = input.strip_prefix("quarkus.datasource.") {
            format!("quarkus.datasource.\"<datasource>\".{rest}")
        } else if input.starts_with("kc.db-") {
            format!("{input}-<datasource>")
        } else {
            input
        };
        Ok(Some(Value::Object(Some(ctx.create_string(&output)))))
    }

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/DatabasePropertyMappers$Datasources",
        "transformDatasourceTo",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_keycloak_transform_datasource_to,
    );

    fn native_keycloak_property_mapping_has_inferred_value(
        _ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(0)))
    }

    fn native_keycloak_false_boolean(
        _ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(0)))
    }

    fn keycloak_cli_args_property(ctx: &mut dyn NativeContext) -> String {
        let key = ctx.create_string("kc.config.args");
        match ctx.invoke(
            "java/lang/System",
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(key))],
        ) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn keycloak_cli_args(ctx: &mut dyn NativeContext) -> Vec<String> {
        let raw = keycloak_cli_args_property(ctx);
        if raw.is_empty() {
            return Vec::new();
        }
        let mut result = Vec::new();
        let mut escaped = false;
        let mut arg = String::new();
        for ch in raw.chars() {
            if ch == ',' {
                if escaped {
                    arg.push(ch);
                }
                escaped = !escaped;
            } else if ch == ' ' {
                if escaped {
                    result.push(std::mem::take(&mut arg));
                    escaped = false;
                } else {
                    arg.push(ch);
                }
            } else {
                arg.push(ch);
            }
        }
        if !arg.is_empty() || !result.is_empty() {
            result.push(arg);
        }
        result
    }

    fn keycloak_cli_value(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
        let prefix = format!("--{}=", key);
        keycloak_cli_args(ctx)
            .into_iter()
            .rev()
            .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_string))
    }

    fn keycloak_cli_bool(ctx: &mut dyn NativeContext, key: &str) -> Option<bool> {
        keycloak_cli_value(ctx, key).map(|v| v.eq_ignore_ascii_case("true"))
    }

    fn keycloak_log_handler_enabled(ctx: &mut dyn NativeContext, handler: &str) -> bool {
        let handlers = keycloak_cli_value(ctx, "log").unwrap_or_else(|| "console".to_string());
        handlers
            .split(',')
            .any(|h| h.trim().eq_ignore_ascii_case(handler))
    }

    fn keycloak_log_async_enabled(
        ctx: &mut dyn NativeContext,
        handler: &str,
        handler_key: &str,
    ) -> bool {
        keycloak_log_handler_enabled(ctx, handler)
            && keycloak_cli_bool(ctx, handler_key)
                .or_else(|| keycloak_cli_bool(ctx, "log-async"))
                .unwrap_or(false)
    }

    fn keycloak_log_output_json(
        ctx: &mut dyn NativeContext,
        handler: &str,
        output_key: &str,
    ) -> bool {
        keycloak_log_handler_enabled(ctx, handler)
            && keycloak_cli_value(ctx, output_key)
                .map(|v| v.eq_ignore_ascii_case("json"))
                .unwrap_or(false)
    }

    fn native_keycloak_log_console_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_handler_enabled(ctx, "console") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_file_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_handler_enabled(ctx, "file") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_syslog_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_handler_enabled(ctx, "syslog") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_console_async_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_async_enabled(ctx, "console", "log-console-async") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_file_async_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_async_enabled(ctx, "file", "log-file-async") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_syslog_async_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_async_enabled(ctx, "syslog", "log-syslog-async") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_console_json_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_output_json(ctx, "console", "log-console-output") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_file_json_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_output_json(ctx, "file", "log-file-output") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_syslog_json_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_output_json(ctx, "syslog", "log-syslog-output") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_file_rotation_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_handler_enabled(ctx, "file")
                && keycloak_cli_bool(ctx, "log-file-rotation-enabled").unwrap_or(false)
            {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_mdc_active(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_cli_bool(ctx, "log-mdc-enabled").unwrap_or(false) {
                1
            } else {
                0
            },
        )))
    }

    fn keycloak_boolean_result(enabled: bool) -> Value {
        Value::Int(if enabled { 1 } else { 0 })
    }

    fn keycloak_metrics_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_cli_bool(ctx, "metrics-enabled").unwrap_or(false)
    }

    fn keycloak_tracing_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_cli_bool(ctx, "tracing-enabled").unwrap_or(false)
    }

    fn keycloak_cache_set_to_infinispan(ctx: &mut dyn NativeContext) -> bool {
        if keycloak_cli_value(ctx, "cache-remote-host").is_some() {
            return false;
        }
        keycloak_cli_value(ctx, "cache")
            .map(|v| v.eq_ignore_ascii_case("ispn"))
            .unwrap_or(true)
    }

    fn keycloak_telemetry_logs_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_cli_bool(ctx, "telemetry-logs-enabled").unwrap_or(false)
    }

    fn keycloak_telemetry_metrics_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_metrics_enabled(ctx)
            && keycloak_cli_bool(ctx, "telemetry-metrics-enabled").unwrap_or(false)
    }

    fn keycloak_telemetry_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_telemetry_logs_enabled(ctx)
            || keycloak_telemetry_metrics_enabled(ctx)
            || keycloak_tracing_enabled(ctx)
    }

    fn native_keycloak_metrics_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(keycloak_metrics_enabled(ctx))))
    }

    fn native_keycloak_cache_set_to_infinispan(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(
            keycloak_cache_set_to_infinispan(ctx),
        )))
    }

    fn native_keycloak_tracing_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(keycloak_tracing_enabled(ctx))))
    }

    fn native_keycloak_tracing_infinispan_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(
            keycloak_tracing_enabled(ctx) && keycloak_cache_set_to_infinispan(ctx),
        )))
    }

    fn native_keycloak_telemetry_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(keycloak_telemetry_enabled(
            ctx,
        ))))
    }

    fn native_keycloak_telemetry_logs_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(
            keycloak_telemetry_logs_enabled(ctx),
        )))
    }

    fn native_keycloak_telemetry_metrics_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(
            keycloak_telemetry_metrics_enabled(ctx),
        )))
    }

    fn native_jaxrs_multivalued_map_add(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let value = args.get(2).copied().unwrap_or(Value::Object(None));
        let store = match ctx.get_field_by_name(this, "store") {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        };

        let base_pin = ctx.pin_native_root(this);
        let store_pin = ctx.pin_native_root(store);
        let key_pin = match key {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
            _ => None,
        };
        let value_pin = match value {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
            _ => None,
        };

        let current_key = match (key, key_pin) {
            (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            _ => key,
        };
        let current_store = ctx.read_native_pin(store_pin, store);
        let list_value = ctx.invoke_virtual(
            current_store,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[current_key],
        )?;

        let list = match list_value {
            Some(Value::Object(Some(o))) => o,
            _ => {
                let new_list =
                    match ctx.new_object_initialized("java/util/LinkedList", "()V", &[])? {
                        Some(Value::Object(Some(o))) => o,
                        _ => {
                            ctx.unpin_native_roots(base_pin);
                            return Ok(None);
                        }
                    };
                let list_pin = ctx.pin_native_root(new_list);
                let current_key = match (key, key_pin) {
                    (Value::Object(Some(o)), Some(pin)) => {
                        Value::Object(Some(ctx.read_native_pin(pin, o)))
                    }
                    _ => key,
                };
                let current_store = ctx.read_native_pin(store_pin, store);
                let current_list = ctx.read_native_pin(list_pin, new_list);
                let _ = ctx.invoke_virtual(
                    current_store,
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[current_key, Value::Object(Some(current_list))],
                )?;
                ctx.read_native_pin(list_pin, new_list)
            }
        };
        let list_pin = ctx.pin_native_root(list);

        let current_value = match (value, value_pin) {
            (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            _ => value,
        };
        let current_list = ctx.read_native_pin(list_pin, list);
        if matches!(current_value, Value::Object(None)) {
            let current_this = ctx.read_native_pin(base_pin, this);
            let _ = ctx.invoke_virtual(
                current_this,
                "addNull",
                "(Ljava/util/List;)V",
                &[Value::Object(Some(current_list))],
            )?;
        } else {
            let _ = ctx.invoke_virtual(
                current_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[current_value],
            )?;
        }
        ctx.unpin_native_roots(base_pin);
        Ok(None)
    }

    fn picocli_annotation_desc_is_member(desc: &str) -> bool {
        matches!(
            desc,
            "Lpicocli/CommandLine$Option;"
                | "Lpicocli/CommandLine$Parameters;"
                | "Lpicocli/CommandLine$ArgGroup;"
                | "Lpicocli/CommandLine$Unmatched;"
                | "Lpicocli/CommandLine$Mixin;"
                | "Lpicocli/CommandLine$Spec;"
                | "Lpicocli/CommandLine$ParentCommand;"
        )
    }

    fn native_picocli_typed_member_is_annotated(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let element = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let element_class = ctx.class_name_of_id(ctx.class_id_of_object(element));
        match element_class.as_deref() {
            Some("java/lang/reflect/Field") => {
                if let Some((class_id, field_name)) =
                    crate::lang_class::field_class_and_name(ctx, element)
                {
                    let present = ctx
                        .field_annotations(class_id, &field_name)
                        .iter()
                        .any(|ann| picocli_annotation_desc_is_member(&ann.type_descriptor));
                    return Ok(Some(Value::Int(if present { 1 } else { 0 })));
                }
            }
            Some("java/lang/reflect/Method") | Some("java/lang/reflect/Constructor") => {
                if let Some((class_id, method_name, method_desc)) =
                    crate::lang_class::method_class_name_desc(ctx, element)
                {
                    let present = ctx
                        .method_annotations(class_id, &method_name, &method_desc)
                        .iter()
                        .any(|ann| picocli_annotation_desc_is_member(&ann.type_descriptor));
                    return Ok(Some(Value::Int(if present { 1 } else { 0 })));
                }
            }
            _ => {}
        }

        let annotation_classes = [
            "picocli/CommandLine$Option",
            "picocli/CommandLine$Parameters",
            "picocli/CommandLine$ArgGroup",
            "picocli/CommandLine$Unmatched",
            "picocli/CommandLine$Mixin",
            "picocli/CommandLine$Spec",
            "picocli/CommandLine$ParentCommand",
        ];
        let element_pin = ctx.pin_native_root(element);
        for annotation_class in annotation_classes {
            let mirror = match ctx.ensure_class_initialized(annotation_class) {
                Ok(class_id) => ctx.get_class_mirror(class_id),
                Err(_) => continue,
            };
            let mirror_pin = ctx.pin_native_root(mirror);
            let current_element = ctx.read_native_pin(element_pin, element);
            let current_mirror = ctx.read_native_pin(mirror_pin, mirror);
            let result = ctx.invoke_virtual(
                current_element,
                "isAnnotationPresent",
                "(Ljava/lang/Class;)Z",
                &[Value::Object(Some(current_mirror))],
            )?;
            if matches!(result, Some(Value::Int(v)) if v != 0) {
                ctx.unpin_native_roots(element_pin);
                return Ok(Some(Value::Int(1)));
            }
        }
        ctx.unpin_native_roots(element_pin);
        Ok(Some(Value::Int(0)))
    }

    fn native_picocli_usage_interpolate_string(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(args.get(1).copied()),
        };
        let text = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let value = ctx.read_string(text).unwrap_or_default();
        if !value.contains("${") {
            return Ok(Some(Value::Object(Some(text))));
        }
        let interpolator = match ctx.get_field_by_name(this, "interpolator") {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Object(Some(text)))),
        };
        let text_pin = ctx.pin_native_root(text);
        let interpolator_pin = ctx.pin_native_root(interpolator);
        let current_interpolator = ctx.read_native_pin(interpolator_pin, interpolator);
        let current_text = ctx.read_native_pin(text_pin, text);
        let result = ctx.invoke_virtual(
            current_interpolator,
            "interpolate",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(current_text))],
        );
        ctx.unpin_native_roots(text_pin);
        result
    }

    fn native_picocli_model_is_non_default(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let value = args.first().copied().unwrap_or(Value::Object(None));
        let default_value = args.get(1).copied().unwrap_or(Value::Object(None));
        if value == default_value {
            return Ok(Some(Value::Int(0)));
        }
        let default_obj = match default_value {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Int(1))),
        };
        if let (Value::Object(Some(value_obj)), Some(default_string)) =
            (value, ctx.read_string(default_obj))
        {
            if let Some(value_string) = ctx.read_string(value_obj) {
                return Ok(Some(Value::Int(if value_string != default_string {
                    1
                } else {
                    0
                })));
            }
        }

        let default_pin = ctx.pin_native_root(default_obj);
        let value_pin = match value {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
            _ => None,
        };
        let current_default = ctx.read_native_pin(default_pin, default_obj);
        let current_value = match (value, value_pin) {
            (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            _ => value,
        };
        let equals = ctx.invoke_virtual(
            current_default,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[current_value],
        )?;
        ctx.unpin_native_roots(default_pin);
        Ok(Some(Value::Int(match equals {
            Some(Value::Int(v)) if v != 0 => 0,
            _ => 1,
        })))
    }

    fn native_picocli_model_array_is_non_default(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let value = args.first().copied().unwrap_or(Value::Object(None));
        let default_value = args.get(1).copied().unwrap_or(Value::Object(None));
        if value == default_value {
            return Ok(Some(Value::Int(0)));
        }
        if matches!(default_value, Value::Object(None)) {
            return Ok(Some(Value::Int(1)));
        }
        let equals = ctx.invoke(
            "java/util/Arrays",
            "equals",
            "([Ljava/lang/Object;[Ljava/lang/Object;)Z",
            &[default_value, value],
        )?;
        Ok(Some(Value::Int(match equals {
            Some(Value::Int(v)) if v != 0 => 0,
            _ => 1,
        })))
    }

    fn keycloak_similarity_bigram_frequency(s: &str) -> rustc_hash::FxHashMap<(u16, u16), i32> {
        let units: Vec<u16> = s.encode_utf16().collect();
        let mut freq = rustc_hash::FxHashMap::default();
        if units.len() < 2 {
            return freq;
        }
        for pair in units.windows(2) {
            *freq.entry((pair[0], pair[1])).or_insert(0) += 1;
        }
        freq
    }

    fn keycloak_similarity_dot(
        left: &rustc_hash::FxHashMap<(u16, u16), i32>,
        right: &rustc_hash::FxHashMap<(u16, u16), i32>,
    ) -> f64 {
        left.iter()
            .map(|(key, value)| (*value as f64) * (*right.get(key).unwrap_or(&0) as f64))
            .sum()
    }

    fn keycloak_cosine_similarity(
        lower_input_freq: &rustc_hash::FxHashMap<(u16, u16), i32>,
        candidate: &str,
    ) -> f64 {
        let candidate_lower = candidate.to_lowercase();
        let candidate_freq = keycloak_similarity_bigram_frequency(&candidate_lower);
        let dot = keycloak_similarity_dot(lower_input_freq, &candidate_freq);
        let norm_input = keycloak_similarity_dot(lower_input_freq, lower_input_freq);
        let norm_candidate = keycloak_similarity_dot(&candidate_freq, &candidate_freq);
        let denominator = (norm_input * norm_candidate).sqrt();
        if denominator == 0.0 {
            0.0
        } else {
            dot / denominator
        }
    }

    fn keycloak_similarity_result_list(
        ctx: &mut dyn NativeContext,
        input: String,
        candidates: ObjectRef,
        max_suggestions: usize,
        min_similarity: f64,
    ) -> MethodCallResult {
        let lower_input = input.to_lowercase();
        let lower_input_freq = keycloak_similarity_bigram_frequency(&lower_input);
        let count = picocli_list_size(ctx, candidates).min(100_000);
        let mut scored: Vec<(f64, usize, String)> = Vec::new();
        for idx in 0..count {
            let Some(candidate_obj) = picocli_list_get(ctx, candidates, idx) else {
                continue;
            };
            let Some(candidate) = ctx.read_string(candidate_obj) else {
                continue;
            };
            let score = keycloak_cosine_similarity(&lower_input_freq, &candidate);
            if score >= min_similarity {
                scored.push((score, idx, candidate));
            }
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });

        let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(list))) => list,
            _ => return Ok(Some(Value::Object(None))),
        };
        let list_pin = ctx.pin_native_root(list);
        for (_, _, candidate) in scored.into_iter().take(max_suggestions) {
            let candidate_string = ctx.create_string(&candidate);
            let string_pin = ctx.pin_native_root(candidate_string);
            let current_list = ctx.read_native_pin(list_pin, list);
            let current_string = ctx.read_native_pin(string_pin, candidate_string);
            let _ = ctx.invoke_virtual(
                current_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(current_string))],
            )?;
            ctx.unpin_native_roots(string_pin);
        }
        let list = ctx.read_native_pin(list_pin, list);
        ctx.unpin_native_roots(list_pin);
        Ok(Some(Value::Object(Some(list))))
    }

    fn native_keycloak_similarity_find_similar_default(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input = match args.first() {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let candidates = match args.get(1) {
            Some(Value::Object(Some(list))) => *list,
            _ => return keycloak_empty_array_list(ctx),
        };
        keycloak_similarity_result_list(ctx, input, candidates, 5, 0.4)
    }

    fn native_keycloak_similarity_find_similar(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input = match args.first() {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let candidates = match args.get(1) {
            Some(Value::Object(Some(list))) => *list,
            _ => return keycloak_empty_array_list(ctx),
        };
        let max_suggestions = match args.get(2) {
            Some(Value::Int(v)) if *v > 0 => *v as usize,
            _ => 0,
        };
        let min_similarity = match args.get(3) {
            Some(Value::Double(v)) => *v,
            Some(Value::Float(v)) => *v as f64,
            _ => 0.0,
        };
        keycloak_similarity_result_list(ctx, input, candidates, max_suggestions, min_similarity)
    }

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/PropertyMapper",
        "getEnabledWhen",
        "()Ljava/util/Optional;",
        native_keycloak_property_mapper_get_enabled_when,
    );

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/PropertyMapper",
        "getRequiredWhen",
        "()Ljava/util/Optional;",
        native_keycloak_property_mapper_get_required_when,
    );

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isConsoleEnabled",
        "()Z",
        native_keycloak_log_console_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isFileEnabled",
        "()Z",
        native_keycloak_log_file_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isSyslogEnabled",
        "()Z",
        native_keycloak_log_syslog_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isConsoleAsyncEnabled",
        "()Z",
        native_keycloak_log_console_async_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isFileAsyncEnabled",
        "()Z",
        native_keycloak_log_file_async_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isSyslogAsyncEnabled",
        "()Z",
        native_keycloak_log_syslog_async_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isConsoleJsonEnabled",
        "()Z",
        native_keycloak_log_console_json_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isFileJsonEnabled",
        "()Z",
        native_keycloak_log_file_json_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isSyslogJsonEnabled",
        "()Z",
        native_keycloak_log_syslog_json_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isFileRotationEnabled",
        "()Z",
        native_keycloak_log_file_rotation_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/MetricsPropertyMappers",
        "metricsEnabled",
        "()Z",
        native_keycloak_metrics_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/CachingPropertyMappers",
        "cacheSetToInfinispan",
        "()Z",
        native_keycloak_cache_set_to_infinispan,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/TelemetryPropertyMappers",
        "isTelemetryEnabled",
        "()Z",
        native_keycloak_telemetry_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/TelemetryPropertyMappers",
        "isTelemetryLogsEnabled",
        "()Z",
        native_keycloak_telemetry_logs_enabled,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/TelemetryPropertyMappers",
        "isTelemetryMetricsEnabled",
        "()Z",
        native_keycloak_telemetry_metrics_enabled,
    );
    r.register(
        "jakarta/ws/rs/core/AbstractMultivaluedMap",
        "add",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        native_jaxrs_multivalued_map_add,
    );
    r.register(
        "picocli/CommandLine$Model$TypedMember",
        "isAnnotated",
        "(Ljava/lang/reflect/AnnotatedElement;)Z",
        native_picocli_typed_member_is_annotated,
    );
    r.register(
        "picocli/CommandLine$Model$UsageMessageSpec",
        "interpolate",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_picocli_usage_interpolate_string,
    );
    r.register(
        "picocli/CommandLine$Model",
        "isNonDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_picocli_model_is_non_default,
    );
    r.register(
        "picocli/CommandLine$Model",
        "isNonDefault",
        "([Ljava/lang/Object;[Ljava/lang/Object;)Z",
        native_picocli_model_array_is_non_default,
    );

    r.register(
        "org/keycloak/quarkus/runtime/configuration/SimilarityUtil",
        "findSimilar",
        "(Ljava/lang/String;Ljava/util/List;)Ljava/util/List;",
        native_keycloak_similarity_find_similar_default,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/SimilarityUtil",
        "findSimilar",
        "(Ljava/lang/String;Ljava/util/List;ID)Ljava/util/List;",
        native_keycloak_similarity_find_similar,
    );

    // --- picocli CommandLine$Help$Ansi$Style.fg(String) / .bg(String) ---
    // Round 92: Keycloak's startup banner contains markup like `@|red ...|@`,
    // which picocli parses via `Ansi.string` -> `Style.parse` -> `Style.fg("red")`.
    // The Java code does:
    //   try { return Style.valueOf(name.toLowerCase()); }   // "red" -> IAE
    //   catch (Exception ex) {
    //     try { return Style.valueOf("fg_" + name.toLowerCase()); }  // "fg_red" -> hit
    //     catch (Exception ex2) { return new Palette256Color(true, name); }
    //   }
    // Under CratonVM the inner-most `IllegalArgumentException` thrown by
    // `Enum.valueOf` propagates out of `Style.fg` rather than being caught by
    // the surrounding `catch (Exception)` handler — exception-table walking
    // for nested invocations across this many frames misroutes the unwind.
    // Implementing `fg`/`bg` natively bypasses the try/catch entirely: we
    // resolve the static Style constant by name and return it directly. If
    // neither lookup hits, we return the canonical `reset` Style as a
    // harmless no-op — Keycloak's banner rendering only needs a non-null
    // IStyle that produces no ANSI codes when used with Ansi.OFF.
    fn picocli_style_lookup(
        ctx: &mut dyn NativeContext,
        prefix: &str,
        raw_name: cratonvm_types::ObjectRef,
    ) -> cratonvm_types::Value {
        let style_cls = "picocli/CommandLine$Help$Ansi$Style";
        let class_id = match ctx.ensure_class_initialized(style_cls) {
            Ok(id) => id,
            Err(_) => {
                if crate::nbflags().dbg_picocli_style {
                    eprintln!("[picocli-style] ensure_class_initialized failed");
                }
                return cratonvm_types::Value::Object(None);
            }
        };
        let name = ctx.read_string(raw_name).unwrap_or_default().to_lowercase();
        let dbg = crate::nbflags().dbg_picocli_style;
        // Round 93: Style constants are STATIC enum fields — must use
        // `static_field_index_by_name`, not `resolve_field_index` (which
        // only walks INSTANCE fields). Round 92's fallback was reaching
        // the `Object(None)` branch on every call, leaking null IStyle
        // to `Ansi.string`, which then NPEs in `Style.on()`.
        // Try the plain name first (e.g. "reset", "bold").
        if let Some(idx) = ctx.static_field_index_by_name(class_id, &name) {
            let v = ctx.get_static_field(class_id, idx);
            if dbg {
                eprintln!(
                    "[picocli-style] plain {} -> idx={} val_null={}",
                    name,
                    idx,
                    matches!(v, cratonvm_types::Value::Object(None))
                );
            }
            if !matches!(v, cratonvm_types::Value::Object(None)) {
                return v;
            }
        } else if dbg {
            eprintln!("[picocli-style] plain {} -> no static field", name);
        }
        // Then the prefixed form (e.g. "fg_red", "bg_blue").
        let prefixed = format!("{}{}", prefix, name);
        if let Some(idx) = ctx.static_field_index_by_name(class_id, &prefixed) {
            let v = ctx.get_static_field(class_id, idx);
            if dbg {
                eprintln!(
                    "[picocli-style] prefixed {} -> idx={} val_null={}",
                    prefixed,
                    idx,
                    matches!(v, cratonvm_types::Value::Object(None))
                );
            }
            if !matches!(v, cratonvm_types::Value::Object(None)) {
                return v;
            }
        } else if dbg {
            eprintln!("[picocli-style] prefixed {} -> no static field", prefixed);
        }
        // Fallback: return the always-present `reset` style. Any IStyle is
        // legal here — Ansi.OFF.string strips markup wholesale anyway.
        if let Some(idx) = ctx.static_field_index_by_name(class_id, "reset") {
            let v = ctx.get_static_field(class_id, idx);
            if dbg {
                eprintln!(
                    "[picocli-style] reset fallback idx={} val_null={}",
                    idx,
                    matches!(v, cratonvm_types::Value::Object(None))
                );
            }
            return v;
        }
        if dbg {
            eprintln!("[picocli-style] reset static field not found, returning null");
        }
        cratonvm_types::Value::Object(None)
    }
    r.register(
        "picocli/CommandLine$Help$Ansi$Style",
        "fg",
        "(Ljava/lang/String;)Lpicocli/CommandLine$Help$Ansi$IStyle;",
        |ctx, args| {
            let name_obj = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(picocli_style_lookup(ctx, "fg_", name_obj)))
        },
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$Style",
        "bg",
        "(Ljava/lang/String;)Lpicocli/CommandLine$Help$Ansi$IStyle;",
        |ctx, args| {
            let name_obj = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(picocli_style_lookup(ctx, "bg_", name_obj)))
        },
    );

    fn picocli_regex_word_hyphen(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    }

    fn picocli_regex_transform(input: &str, synopsis: bool) -> String {
        if let Some(rest) = input.strip_prefix("--no-") {
            if picocli_regex_word_hyphen(rest) {
                return if synopsis {
                    format!("--[no-]{}", rest)
                } else {
                    format!("--{}", rest)
                };
            }
        }
        if let Some(rest) = input.strip_prefix("--") {
            if picocli_regex_word_hyphen(rest) {
                return if synopsis {
                    format!("--[no-]{}", rest)
                } else {
                    format!("--no-{}", rest)
                };
            }
        }

        let (prefix, rest) = if let Some(rest) = input.strip_prefix("--") {
            ("--", rest)
        } else if let Some(rest) = input.strip_prefix('-') {
            ("-", rest)
        } else {
            return input.to_string();
        };
        let Some(colon) = rest.find(':') else {
            return input.to_string();
        };
        if !rest[..colon]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return input.to_string();
        }
        let after_colon = &rest[colon + 1..];
        let (replacement_sign, value) = if let Some(value) = after_colon.strip_prefix('+') {
            ('-', value)
        } else if let Some(value) = after_colon.strip_prefix('-') {
            ('+', value)
        } else {
            return input.to_string();
        };
        if !picocli_regex_word_hyphen(value) {
            return input.to_string();
        }
        if synopsis {
            format!("{}{}:(+|-){}", prefix, &rest[..colon], value)
        } else {
            format!("{}{}:{}{}", prefix, &rest[..colon], replacement_sign, value)
        }
    }

    fn picocli_new_regex_transformer(ctx: &mut dyn NativeContext) -> MethodCallResult {
        let obj = alloc_concurrent_synthetic(ctx, "picocli/CommandLine$RegexTransformer", 2);
        // Pin across the emptyMap invoke below — a moving young GC there would
        // relocate the fresh transformer (native stale-local family).
        let obj_pin = ctx.pin_native_root(obj);
        if let Ok(Some(empty_map)) = ctx.invoke(
            "java/util/Collections",
            "emptyMap",
            "()Ljava/util/Map;",
            &[],
        ) {
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field_by_name(obj, "replacements", empty_map);
            ctx.set_field_by_name(obj, "synopsis", empty_map);
        }
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.unpin_native_roots(obj_pin);
        Ok(Some(Value::Object(Some(obj))))
    }

    fn native_picocli_regex_transform_negative(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let out = ctx.create_string(&picocli_regex_transform(&input, false));
        Ok(Some(Value::Object(Some(out))))
    }

    fn native_picocli_regex_transform_synopsis(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let out = ctx.create_string(&picocli_regex_transform(&input, true));
        Ok(Some(Value::Object(Some(out))))
    }

    r.register(
        "picocli/CommandLine$RegexTransformer",
        "createDefault",
        "()Lpicocli/CommandLine$RegexTransformer;",
        |ctx, _args| picocli_new_regex_transformer(ctx),
    );
    r.register(
        "picocli/CommandLine$RegexTransformer",
        "createCaseInsensitive",
        "()Lpicocli/CommandLine$RegexTransformer;",
        |ctx, _args| picocli_new_regex_transformer(ctx),
    );
    r.register(
        "picocli/CommandLine$RegexTransformer",
        "makeNegative",
        "(Ljava/lang/String;Lpicocli/CommandLine$Model$CommandSpec;)Ljava/lang/String;",
        native_picocli_regex_transform_negative,
    );
    r.register(
        "picocli/CommandLine$RegexTransformer",
        "makeSynopsis",
        "(Ljava/lang/String;Lpicocli/CommandLine$Model$CommandSpec;)Ljava/lang/String;",
        native_picocli_regex_transform_synopsis,
    );

    r.register(
        "java/util/Arrays$ArrayList",
        "<init>",
        "([Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let array = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "a", array);
            Ok(None)
        },
    );
    r.register(
        "java/util/Arrays$ArrayList",
        "toArray",
        "()[Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let backing = match ctx.get_field_by_name(this, "a") {
                Value::Object(Some(arr)) => arr,
                _ => {
                    let empty = ctx.new_ref_array(ClassId::new(0), 0);
                    return Ok(Some(Value::Object(Some(empty))));
                }
            };
            let len = ctx.array_length(backing);
            let out = ctx.new_ref_array(ClassId::new(0), len);
            for i in 0..len {
                let value = ctx.get_array_element(backing, i);
                ctx.set_array_element(out, i, value);
            }
            Ok(Some(Value::Object(Some(out))))
        },
    );

    r.register(
        "picocli/CommandLine$Model$CaseAwareLinkedMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let key_for_lookup = if matches!(ctx.get_field_by_name(this, "caseInsensitive"), Value::Int(v) if v != 0) {
                match key {
                    Value::Object(Some(key_obj))
                        if ctx
                            .class_name_of_id(ctx.class_id_of_object(key_obj))
                            .as_deref()
                            == Some("java/lang/String") =>
                    {
                        let lowered = ctx
                            .read_string(key_obj)
                            .unwrap_or_default()
                            .to_ascii_lowercase();
                        Value::Object(Some(ctx.create_string(&lowered)))
                    }
                    Value::Object(None) => Value::Object(None),
                    _ => return Ok(Some(Value::Int(0))),
                }
            } else {
                key
            };
            let map_field = if matches!(ctx.get_field_by_name(this, "caseInsensitive"), Value::Int(v) if v != 0) {
                "keyMap"
            } else {
                "targetMap"
            };
            let map = match picocli_obj_field(ctx, this, map_field) {
                Some(map) => map,
                None => return Ok(Some(Value::Int(0))),
            };
            ctx.invoke_virtual(
                map,
                "containsKey",
                "(Ljava/lang/Object;)Z",
                &[key_for_lookup],
            )
        },
    );

    // --- Files extras ---
    r.register(
        files,
        "isDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| Ok(Some(Value::Int(i32::from(p57_files_is_directory_impl(ctx, args))))),
    );

    r.register(
        files,
        "isRegularFile",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            Ok(Some(Value::Int(i32::from(p57_files_is_regular_file_impl(
                ctx, args,
            )))))
        },
    );

    r.register(
        files,
        "isSymbolicLink",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            let is_link = jrtfs_decode(&p)
                .and_then(|(java_home, entry)| {
                    jrt_image(&java_home).and_then(|image| {
                        jrt_package_link_target(&image, &entry.strip_prefix("packages/")?)
                    })
                })
                .is_some()
                || std::path::Path::new(&p).is_symlink();
            Ok(Some(Value::Int(if is_link { 1 } else { 0 })))
        },
    );

    r.register(
        files,
        "readSymbolicLink",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        // Host symlinks and jrt package links both live in
        // `p57_read_symbolic_link`; this used to handle only the jrt case and
        // answered `NoSuchFileException` for every real symbolic link.
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            p57_read_symbolic_link(ctx, &p)
        },
    );

    r.register(
        files,
        "isReadable",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            let readable = match vfs_classify(&p) {
                Some(kind) => !matches!(kind, JarFsKind::Absent),
                None => std::path::Path::new(&p).exists(),
            };
            Ok(Some(Value::Int(if readable { 1 } else { 0 })))
        },
    );

    r.register(
        files,
        "isWritable",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            // Was existence-only (ignored real permission bits entirely), so
            // `Files.isWritable` never reflected `Files.setPosixFilePermissions`/
            // `PosixFileAttributeView.setPermissions` clearing the write bits —
            // H2's `FilePathDisk.canWrite()` calls this directly, so
            // `TestFileSystem.testSetReadOnly`'s `assertFalse(canWrite(...))`
            // would still fail even once `setReadOnly()` itself stopped
            // throwing. Mirror `File.canWrite()`'s real permission check
            // (same `Permissions::readonly()` query) instead.
            let writable = std::fs::metadata(&p)
                .map(|m| !m.permissions().readonly())
                .unwrap_or(false);
            Ok(Some(Value::Int(if writable { 1 } else { 0 })))
        },
    );

    r.register(files, "size", "(Ljava/nio/file/Path;)J", p59_files_size);

    // `Files.list` / `Files.walk` return a `Stream<Path>` built (in the real
    // JDK) by wrapping `newDirectoryStream(dir).iterator()` in a
    // `Spliterators.spliteratorUnknownSize` Stream. On CratonVM that real
    // bytecode path does not reach the synthetic `newDirectoryStream` provider
    // native for non-default (jar/jrt) filesystems, so the resulting Stream is
    // empty — which broke in-process javac (it enumerates the runtime image's
    // packages via `Files.list`/`Files.walk` and saw zero, reporting "Unable to
    // find package java.lang in platform classes"). Register these directly so
    // they return a fully-functional eager synthetic Stream over the listing,
    // uniformly for host / jar / jrt paths.
    r.register(
        files,
        "list",
        "(Ljava/nio/file/Path;)Ljava/util/stream/Stream;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            let entries = vfs_or_host_list(&p);
            let mut vals = Vec::with_capacity(entries.len());
            for e in &entries {
                let ep = p57_alloc_path(ctx, e);
                vals.push(Value::Object(Some(ep)));
            }
            cratonvm_native_collections::make_stream_from_elements(ctx, &vals)
        },
    );
    // Files.walk(Path, FileVisitOption...) and the maxDepth overload.
    fn files_walk_stream(
        ctx: &mut dyn NativeContext,
        path_obj: ObjectRef,
        max_depth: usize,
        follow_links: bool,
    ) -> MethodCallResult {
        let p = p57_read_path(ctx, path_obj);
        let mut paths = Vec::new();
        vfs_or_host_walk(&p, 0, max_depth, follow_links, &mut paths);
        let mut vals = Vec::with_capacity(paths.len());
        for e in &paths {
            let ep = p57_alloc_path(ctx, e);
            vals.push(Value::Object(Some(ep)));
        }
        cratonvm_native_collections::make_stream_from_elements(ctx, &vals)
    }
    r.register(
        files,
        "walk",
        "(Ljava/nio/file/Path;[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
        |ctx, args| {
            // Read the options FIRST: the Set form calls back into `isEmpty()`
            // bytecode, which can allocate, so any `ObjectRef` copied out of
            // `args` before it would be a stale local under a moving young GC.
            let follow = p57_visit_options_follow_links(ctx, args.get(1));
            let path_obj = obj_arg(args, 0)?;
            files_walk_stream(ctx, path_obj, usize::MAX, follow)
        },
    );
    r.register(
        files,
        "walk",
        "(Ljava/nio/file/Path;I[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
        |ctx, args| {
            let follow = p57_visit_options_follow_links(ctx, args.get(2));
            let path_obj = obj_arg(args, 0)?;
            let max_depth = match args.get(1) {
                Some(Value::Int(n)) if *n >= 0 => *n as usize,
                _ => usize::MAX,
            };
            files_walk_stream(ctx, path_obj, max_depth, follow)
        },
    );
    r.register(
        files,
        "find",
        "(Ljava/nio/file/Path;ILjava/util/function/BiPredicate;[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
        |ctx, args| {
            // Options first — see the `walk` registration above.
            let follow = p57_visit_options_follow_links(ctx, args.get(3));
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            let max_depth = match args.get(1) {
                Some(Value::Int(n)) if *n >= 0 => *n as usize,
                _ => usize::MAX,
            };
            let matcher = match args.get(2).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            };
            let mut paths = Vec::new();
            vfs_or_host_walk(&p, 0, max_depth, follow, &mut paths);
            // Without FOLLOW_LINKS the JDK's walker reads each entry's
            // attributes with NOFOLLOW_LINKS, which is what lets a `BiPredicate`
            // see `attrs.isSymbolicLink()`. `p59_files_read_attributes` reads
            // that request off a non-empty `LinkOption[]` (the enum has one
            // constant), so hand it a 1-element array when not following.
            let nofollow_opts = if follow {
                Value::Object(None)
            } else {
                Value::Object(Some(
                    ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1),
                ))
            };
            let nofollow_pin = match nofollow_opts {
                Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
                _ => None,
            };
            let mut vals = Vec::with_capacity(paths.len());
            for e in &paths {
                let ep = p57_alloc_path(ctx, e);
                let ep_pin = ctx.pin_native_root(ep);
                let opts = match nofollow_pin {
                    Some((pin, orig)) => Value::Object(Some(ctx.read_native_pin(pin, orig))),
                    None => Value::Object(None),
                };
                let attrs =
                    match p59_files_read_attributes(ctx, &[Value::Object(Some(ep)), opts])? {
                        Some(Value::Object(Some(attrs))) => attrs,
                        _ => {
                            ctx.unpin_native_roots(ep_pin);
                            continue;
                        }
                    };
                let attrs_pin = ctx.pin_native_root(attrs);
                let attrs = ctx.read_native_pin(attrs_pin, attrs);
                let ep_current = ctx.read_native_pin(ep_pin, ep);
                let keep = if let Some(matcher) = matcher {
                    match ctx.invoke_virtual(
                        matcher,
                        "test",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
                        &[Value::Object(Some(ep_current)), Value::Object(Some(attrs))],
                    )? {
                        Some(Value::Int(v)) => v != 0,
                        _ => false,
                    }
                } else {
                    !matches!(ctx.get_field(attrs, 3), Value::Int(v) if v != 0)
                };
                let ep = ctx.read_native_pin(ep_pin, ep);
                ctx.unpin_native_roots(attrs_pin);
                ctx.unpin_native_roots(ep_pin);
                if keep {
                    vals.push(Value::Object(Some(ep)));
                }
            }
            if let Some((pin, _)) = nofollow_pin {
                ctx.unpin_native_roots(pin);
            }
            cratonvm_native_collections::make_stream_from_elements(ctx, &vals)
        },
    );

    r.register(
        files,
        "readString",
        "(Ljava/nio/file/Path;)Ljava/lang/String;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            match p57_read_to_string(&p) {
                Ok(content) => {
                    let s = ctx.create_string(&content);
                    Ok(Some(Value::Object(Some(s))))
                }
                // NIO contract: missing file → NoSuchFileException (see
                // newByteChannel above), not a bare IOException — callers like
                // FileSystemResource.getContentAsString() catch
                // NoSuchFileException and translate it to FileNotFoundException.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p))
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );

    r.register(
        files,
        "readString",
        "(Ljava/nio/file/Path;Ljava/nio/charset/Charset;)Ljava/lang/String;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            // Ignore charset, always UTF-8
            match p57_read_to_string(&p) {
                Ok(content) => {
                    let s = ctx.create_string(&content);
                    Ok(Some(Value::Object(Some(s))))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p))
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );

    r.register(
        files,
        "writeString",
        "(Ljava/nio/file/Path;Ljava/lang/CharSequence;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        files_write_string_impl,
    );

    fn read_all_lines_impl(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        let path_obj = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, path_obj);
        match p57_read_to_string(&p) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                // Resolve real-JDK ArrayList layout: elementData / size slots
                // can be at (1,2) when AbstractList.modCount occupies slot 0.
                // Fall back to synthetic (0,1) layout when the class isn't
                // available via the resolver.
                let data_slot = ctx
                    .resolve_field_index("java/util/ArrayList", "elementData")
                    .unwrap_or(0);
                let size_slot = ctx
                    .resolve_field_index("java/util/ArrayList", "size")
                    .unwrap_or(1);
                let n_fields = std::cmp::max(data_slot, size_slot) + 1;
                let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", n_fields);
                // Pin across the array/string allocs below — a moving young GC
                // there would relocate the fresh list/array (native
                // stale-local family).
                let list_pin = ctx.pin_native_root(list);
                use cratonvm_types::ArrayElementType;
                let arr = ctx.new_array(ArrayElementType::Reference, lines.len());
                let arr_pin = ctx.pin_native_root(arr);
                for (i, line) in lines.iter().enumerate() {
                    let s = ctx.create_string(line);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, Value::Object(Some(s)));
                }
                let list = ctx.read_native_pin(list_pin, list);
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_field(list, data_slot, Value::Object(Some(arr)));
                ctx.set_field(list, size_slot, Value::Int(lines.len() as i32));
                ctx.unpin_native_roots(list_pin);
                Ok(Some(Value::Object(Some(list))))
            }
            Err(e) => Err(p57_io_error(&e)),
        }
    }
    r.register(
        files,
        "readAllLines",
        "(Ljava/nio/file/Path;)Ljava/util/List;",
        read_all_lines_impl,
    );
    r.register(
        files,
        "readAllLines",
        "(Ljava/nio/file/Path;Ljava/nio/charset/Charset;)Ljava/util/List;",
        read_all_lines_impl,
    );

    r.register(
        files,
        "lines",
        "(Ljava/nio/file/Path;)Ljava/util/stream/Stream;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            match p57_read_to_string(&p) {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    use cratonvm_types::ArrayElementType;
                    let arr = ctx.new_array(ArrayElementType::Reference, lines.len());
                    // Pin across the string/stream allocs below — a moving
                    // young GC there would relocate the fresh array (native
                    // stale-local family).
                    let arr_pin = ctx.pin_native_root(arr);
                    for (i, line) in lines.iter().enumerate() {
                        let s = ctx.create_string(line);
                        let arr = ctx.read_native_pin(arr_pin, arr);
                        ctx.set_array_element(arr, i, Value::Object(Some(s)));
                    }
                    let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_field(stream, 0, Value::Object(Some(arr)));
                    ctx.unpin_native_roots(arr_pin);
                    Ok(Some(Value::Object(Some(stream))))
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );

    r.register(files, "createTempFile", "(Ljava/lang/String;Ljava/lang/String;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;", |ctx, args| {
        let prefix_ref = obj_arg(args, 0)?;
        let prefix = ctx.read_string(prefix_ref).unwrap_or_default();
        let suffix = if let Value::Object(Some(s)) = args[1] {
            ctx.read_string(s).unwrap_or_else(|| ".tmp".to_string())
        } else {
            ".tmp".to_string()
        };
        let temp_dir = jdk_temp_dir(ctx.get_system_property("java.io.tmpdir"));
        let path_str = jdk_create_temp_file(&temp_dir, &prefix, &suffix)?;
        // Real JDK: `TempFileHelper.create` creates temp FILES 0600 by
        // default, and honors an explicit `FileAttribute` above that. (The
        // `java.io.File.createTempFile` natives keep the umask default —
        // that API has no such contract.)
        apply_unix_mode(
            &path_str,
            posix_mode_from_file_attributes(ctx, args.get(2)).or(Some(0o600)),
        );
        let result = p57_alloc_path(ctx, &path_str);
        Ok(Some(Value::Object(Some(result))))
    });

    r.register(
        files,
        "createTempDirectory",
        "(Ljava/lang/String;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        |ctx, args| {
            let prefix_ref = obj_arg(args, 0)?;
            let prefix = ctx.read_string(prefix_ref).unwrap_or_default();
            let temp_dir = jdk_temp_dir(ctx.get_system_property("java.io.tmpdir"));
            let name = format!(
                "{}{}",
                prefix,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let full_path = temp_dir.join(name);
            std::fs::create_dir_all(&full_path).map_err(|e| p57_io_error(&e))?;
            let path_str = full_path.to_string_lossy().to_string();
            // Real JDK: `TempFileHelper.create` creates temp directories 0700
            // by default, and honors an explicit `FileAttribute` above that.
            apply_unix_mode(
                &path_str,
                posix_mode_from_file_attributes(ctx, args.get(1)).or(Some(0o700)),
            );
            let result = p57_alloc_path(ctx, &path_str);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    r.register(files, "delete", "(Ljava/nio/file/Path;)V", |ctx, args| {
        let path_obj = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, path_obj);
        p57_delete_path(&p);
        Ok(None)
    });

    r.register(
        files,
        "deleteIfExists",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            // `Path::exists()` follows links, so a DANGLING symlink read as
            // "does not exist" and was left on disk. The link itself is what
            // `deleteIfExists` is being asked about.
            if std::fs::symlink_metadata(&p).is_err() {
                return Ok(Some(Value::Int(0)));
            }
            p57_delete_path(&p);
            Ok(Some(Value::Int(1)))
        },
    );

    r.register(
        files,
        "copy",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)Ljava/nio/file/Path;",
        |ctx, args| {
            // Read the options FIRST: the probe re-enters Java, and a moving
            // young GC there would relocate any `ObjectRef` already lifted out
            // of `args` (the native stale-local family).
            let replace_existing = copy_options_replace_existing(ctx, args.get(2));
            let src = obj_arg(args, 0)?;
            let dst = obj_arg(args, 1)?;
            let src_path = p57_read_path(ctx, src);
            let dst_path = p57_read_path(ctx, dst);
            // Without REPLACE_EXISTING, `Files.copy` onto an existing target
            // must throw `FileAlreadyExistsException` — callers catch it BY
            // TYPE, so a generic IOException will not do. `std::fs::copy`
            // below overwrites unconditionally and the directory arm swallows
            // `AlreadyExists`, so this is the only place the contract can be
            // enforced. The sibling `Files.move` native already does exactly
            // this with the same two helpers. Skip the check for jarfs-encoded
            // targets (not real filesystem paths) and for a self-copy.
            if !replace_existing
                && src_path != dst_path
                && jarfs_decode(&dst_path).is_none()
                && std::fs::symlink_metadata(&dst_path).is_ok()
            {
                return Err(p57_file_already_exists(ctx, &dst_path));
            }
            // Java `Files.copy(Path,Path,CopyOption...)`: copying a DIRECTORY
            // creates an (empty) directory at the target — it does NOT open the
            // source as a file. `std::fs::copy` only handles regular files and on
            // a directory fails ("Access denied / os error 5" on Windows), which
            // broke every `TomcatBaseTest.recursiveCopy` (the whole
            // `catalina.webresources` cluster — `preVisitDirectory` does
            // `Files.copy(dir, …)`). Branch on the source kind; tolerate an
            // already-existing target dir (mirrors the file path's overwrite).
            // Classify the SOURCE too: `src_path` may itself be a jarfs-encoded
            // entry (e.g. a Path from `FileSystems.newFileSystem(zipPath, ...)`,
            // as used by Quarkus's `ZipUtils.unzip`/`copyFromZip` to extract a
            // mounted zip's contents to the real filesystem). Previously this
            // only special-cased a jarfs DESTINATION and always read the source
            // via `std::fs::symlink_metadata`/`std::fs::copy`, which treats the
            // jarfs-encoded source string as a literal OS path — it never is
            // one, so both calls failed with a raw `NotFound` ("No such file or
            // directory (os error 2)"), surfacing as `IllegalStateException:
            // IOException: No such file or directory (os error 2)` even though
            // `Files.isDirectory`/`isRegularFile` on the same Path (which DO
            // classify via `vfs_classify`/`jarfs_classify`) reported correctly.
            let src_jarfs = jarfs_decode(&src_path);
            let src_is_dir = if let Some((ref jar, ref entry)) = src_jarfs {
                matches!(jarfs_classify(jar, entry), JarFsKind::Dir)
            } else {
                std::fs::symlink_metadata(&src_path)
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
            };
            let result = if let Some((dst_jar, dst_entry)) = jarfs_decode(&dst_path) {
                if src_is_dir {
                    jarfs_create_dir_entry(&dst_jar, &dst_entry)
                } else if let Some((src_jar, src_entry)) = &src_jarfs {
                    jarfs_read_entry(src_jar, src_entry)
                        .and_then(|bytes| jarfs_write_file_entry(&dst_jar, &dst_entry, &bytes))
                } else {
                    std::fs::read(&src_path)
                        .and_then(|bytes| jarfs_write_file_entry(&dst_jar, &dst_entry, &bytes))
                }
            } else if src_is_dir {
                match std::fs::create_dir(&dst_path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(e) => Err(e),
                }
            } else if let Some((src_jar, src_entry)) = &src_jarfs {
                jarfs_read_entry(src_jar, src_entry)
                    .and_then(|bytes| std::fs::write(&dst_path, &bytes))
            } else {
                std::fs::copy(&src_path, &dst_path).map(|_| ())
            };
            match result {
                Ok(()) => Ok(Some(Value::Object(Some(dst)))),
                Err(e) => Err(RuntimeError::IllegalStateException {
                    message: format!("IOException: {}", e),
                }
                .into()),
            }
        },
    );

    r.register(
        files,
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)Ljava/nio/file/Path;",
        |ctx, args| {
            let src = obj_arg(args, 0)?;
            let dst = obj_arg(args, 1)?;
            let src_path = p57_read_path(ctx, src);
            let dst_path = p57_read_path(ctx, dst);
            // Real Files.move contract: without REPLACE_EXISTING, throw
            // FileAlreadyExistsException if the target already exists. Plain
            // std::fs::rename is POSIX rename(2) semantics, which silently
            // replaces the destination unconditionally -- so H2's
            // FilePathDisk.moveTo(newName, false) (no REPLACE_EXISTING) never
            // saw the FileAlreadyExistsException it catches to translate into
            // DbException(FILE_RENAME_FAILED_2), letting
            // TestFileSystem.testMoveTo's move-onto-existing-file case
            // through instead of rejecting it (
            // bug-h2-files-setposixfilepermissions-FIXED.md residual chain).
            // ATOMIC_MOVE also implies replacement: it is specified as a single
            // filesystem operation, which on every platform CratonVM targets is
            // `rename(2)` / `MoveFileEx(MOVEFILE_REPLACE_EXISTING)` — the target
            // is replaced, not rejected. Treating it as "no replace" made
            // Spring Boot's Kubernetes ConfigMap atomic-swap case
            // (`FileWatcherTests.shouldTriggerOnConfigMapAtomicMoveUpdates`,
            // which moves a fresh `..data` symlink over the live one) fail with
            // `FileAlreadyExistsException`.
            let mut replace_existing = false;
            if let Some(Value::Object(Some(opts))) = args.get(2) {
                let len = ctx.array_length(*opts);
                for i in 0..len {
                    if let Value::Object(Some(opt)) = ctx.get_array_element(*opts, i) {
                        if let Ok(Some(Value::Object(Some(s)))) =
                            ctx.invoke_virtual(opt, "toString", "()Ljava/lang/String;", &[])
                        {
                            let name = ctx.read_string(s).unwrap_or_default();
                            if name.contains("REPLACE_EXISTING") || name.contains("ATOMIC_MOVE") {
                                replace_existing = true;
                            }
                        }
                    }
                }
            }
            if !replace_existing
                && src_path != dst_path
                && std::fs::symlink_metadata(&dst_path).is_ok()
            {
                // Build a REAL java/nio/file/FileAlreadyExistsException via
                // its real single-String constructor (same pattern as
                // throw_unsupported_charset_exception below) rather than a
                // synthetic layout -- this exception is caught by H2's own
                // real FilePathDisk.moveTo bytecode (catch
                // (FileAlreadyExistsException ex)) and its getFile() may be
                // read by real Throwable formatting, so it needs genuine
                // field layout, not a guessed synthetic one.
                if let Ok(Some(Value::Object(Some(exc)))) =
                    ctx.new_object("java/nio/file/FileAlreadyExistsException")
                {
                    let file_str = ctx.create_string(&dst_path);
                    let _ = ctx.invoke(
                        "java/nio/file/FileAlreadyExistsException",
                        "<init>",
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(exc)), Value::Object(Some(file_str))],
                    );
                    return Err(MethodCallFailed::ExceptionThrown(exc));
                }
            }
            match std::fs::rename(&src_path, &dst_path) {
                Ok(()) => Ok(Some(Value::Object(Some(dst)))),
                Err(e) => Err(RuntimeError::IllegalStateException {
                    message: format!("IOException: {}", e),
                }
                .into()),
            }
        },
    );

    r.register(
        files,
        "createDirectories",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            if let Some((jar, entry)) = jarfs_decode(&p) {
                return match jarfs_create_dir_entry(&jar, &entry) {
                    Ok(()) => Ok(Some(Value::Object(Some(path_obj)))),
                    Err(e) => Err(p57_io_error(&e)),
                };
            }
            // Same `FileAttribute` contract as `createDirectory`. The real
            // JDK applies the mode to each directory it creates; applying it
            // to the leaf covers every caller we have (the parents are
            // usually pre-existing).
            let mode = posix_mode_from_file_attributes(ctx, args.get(1));
            match std::fs::create_dir_all(&p) {
                Ok(()) => {
                    apply_unix_mode(&p, mode);
                    Ok(Some(Value::Object(Some(path_obj))))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p))
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    Err(p57_access_denied(ctx, &p))
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let exc = alloc_concurrent_synthetic(
                        ctx,
                        "java/nio/file/FileAlreadyExistsException",
                        4,
                    );
                    // Pin across the create_string below — a moving young GC
                    // there would relocate the fresh exception (native
                    // stale-local family).
                    let exc_pin = ctx.pin_native_root(exc);
                    let file_str = ctx.create_string(&p);
                    let exc = ctx.read_native_pin(exc_pin, exc);
                    ctx.set_field_by_name(exc, "file", Value::Object(Some(file_str)));
                    ctx.unpin_native_roots(exc_pin);
                    Err(MethodCallFailed::ExceptionThrown(exc))
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );

    r.register(
        files,
        "createDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            if let Some((jar, entry)) = jarfs_decode(&p) {
                return match jarfs_create_dir_entry(&jar, &entry) {
                    Ok(()) => Ok(Some(Value::Object(Some(path_obj)))),
                    Err(e) => Err(p57_io_error(&e)),
                };
            }
            // Honor `PosixFilePermissions.asFileAttribute(...)` (arg 1): the
            // real JDK passes it straight to `mkdir(2)`. Ignoring it left
            // every such directory at the process umask.
            let mode = posix_mode_from_file_attributes(ctx, args.get(1));
            match create_dir_with_mode(&p, mode) {
                Ok(()) => Ok(Some(Value::Object(Some(path_obj)))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p))
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    Err(p57_access_denied(ctx, &p))
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let exc = alloc_concurrent_synthetic(
                        ctx,
                        "java/nio/file/FileAlreadyExistsException",
                        4,
                    );
                    // Pin across the create_string below — a moving young GC
                    // there would relocate the fresh exception (native
                    // stale-local family).
                    let exc_pin = ctx.pin_native_root(exc);
                    let file_str = ctx.create_string(&p);
                    let exc = ctx.read_native_pin(exc_pin, exc);
                    ctx.set_field_by_name(exc, "file", Value::Object(Some(file_str)));
                    ctx.unpin_native_roots(exc_pin);
                    Err(MethodCallFailed::ExceptionThrown(exc))
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );

    // FileVisitResult enum values
    let fvr = "java/nio/file/FileVisitResult";
    r.register(
        fvr,
        "CONTINUE",
        "Ljava/nio/file/FileVisitResult;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0),
    );
    r.register(
        fvr,
        "TERMINATE",
        "Ljava/nio/file/FileVisitResult;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "TERMINATE", 1),
    );
    r.register(
        fvr,
        "SKIP_SUBTREE",
        "Ljava/nio/file/FileVisitResult;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "SKIP_SUBTREE", 2),
    );
    r.register(
        fvr,
        "SKIP_SIBLINGS",
        "Ljava/nio/file/FileVisitResult;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "SKIP_SIBLINGS", 3),
    );

    // StandardOpenOption enum
    let soo = "java/nio/file/StandardOpenOption";
    r.register(
        soo,
        "READ",
        "Ljava/nio/file/StandardOpenOption;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/StandardOpenOption", "READ", 0),
    );
    r.register(
        soo,
        "WRITE",
        "Ljava/nio/file/StandardOpenOption;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/StandardOpenOption", "WRITE", 1),
    );
    r.register(
        soo,
        "APPEND",
        "Ljava/nio/file/StandardOpenOption;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/StandardOpenOption", "APPEND", 2),
    );
    r.register(
        soo,
        "CREATE",
        "Ljava/nio/file/StandardOpenOption;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/StandardOpenOption", "CREATE", 3),
    );
    r.register(
        soo,
        "CREATE_NEW",
        "Ljava/nio/file/StandardOpenOption;",
        |ctx, _args| p57_alloc_enum(ctx, "java/nio/file/StandardOpenOption", "CREATE_NEW", 4),
    );
    r.register(
        soo,
        "TRUNCATE_EXISTING",
        "Ljava/nio/file/StandardOpenOption;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/StandardOpenOption",
                "TRUNCATE_EXISTING",
                6,
            )
        },
    );

    // --- FileSystemProvider file I/O methods ---
    r.register(
        fsp,
        "newByteChannel",
        "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/SeekableByteChannel;",
        |ctx, args| {
            let path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, path_obj);
            // Jar-filesystem entries are not real OS files (`fd_table` cannot
            // open them), so keep the in-memory snapshot path for them. (These
            // remain read-only via the methodless stub — unchanged behaviour.)
            if let Some(read) = vfs_read(&p) {
                return match read {
                    Ok(data) => {
                        let channel = alloc_concurrent_synthetic(
                            ctx,
                            "java/nio/channels/SeekableByteChannel",
                            3,
                        );
                        // Pin across the array alloc below — a moving young GC
                        // there would relocate the fresh channel (native
                        // stale-local family).
                        let channel_pin = ctx.pin_native_root(channel);
                        use cratonvm_types::ArrayElementType;
                        let arr = ctx.new_array(ArrayElementType::Byte, data.len());
                        let channel = ctx.read_native_pin(channel_pin, channel);
                        ctx.unpin_native_roots(channel_pin);
                        for (i, &b) in data.iter().enumerate() {
                            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                        }
                        ctx.set_field(channel, 0, Value::Object(Some(arr))); // data
                        ctx.set_field(channel, 1, Value::Int(0)); // position
                        ctx.set_field(channel, 2, Value::Int(data.len() as i32)); // size
                        Ok(Some(Value::Object(Some(channel))))
                    }
                    // NIO contract: a missing file must surface as
                    // `java.nio.file.NoSuchFileException`, NOT a bare `IOException`.
                    // Frameworks treat config sources as OPTIONAL by catching
                    // NoSuchFileException (e.g. SmallRye Config loading Keycloak's
                    // profile-specific `keycloak-<profile>.conf`); a generic
                    // IOException escapes that catch and aborts boot (SRCFG00035).
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Err(p57_no_such_file(ctx, &p))
                    }
                    Err(e) => Err(p57_io_error(&e)),
                };
            }
            // NOFOLLOW_LINKS has to be answered BEFORE the missing-file
            // pre-check below, because the two disagree about a *dangling*
            // symlink: `Path::exists()` is a `stat`, so it reports the link as
            // absent, and without this the pre-check would return
            // `NoSuchFileException` — which callers legitimately catch and
            // recover from — where the kernel's `O_NOFOLLOW` would have said
            // `ELOOP`. The delegation to `newFileChannel` below carries the same
            // check for every other case; this one has to be here.
            if fsp_scan_open_options(ctx, args.get(2).copied()).nofollow {
                if let Some(refused) = p57_nofollow_reject(&p) {
                    return Err(refused);
                }
            }
            // Preserve the NIO missing-file contract: opening a non-existent
            // path for READ — or for WRITE without CREATE/CREATE_NEW — must throw
            // `java.nio.file.NoSuchFileException`, which frameworks catch to treat
            // a config source as OPTIONAL (SmallRye loading Keycloak's
            // `keycloak-<profile>.conf`; SRCFG00035). `newFileChannel`'s fd open
            // would surface a generic `IOException` here, so pre-check.
            if !std::path::Path::new(&p).exists() {
                let creates = match args.get(2) {
                    Some(Value::Object(Some(set))) => ctx
                        .invoke_virtual(*set, "toString", "()Ljava/lang/String;", &[])
                        .ok()
                        .flatten()
                        .and_then(|v| match v {
                            Value::Object(Some(s)) => ctx.read_string(s),
                            _ => None,
                        })
                        .map(|s| s.contains("CREATE"))
                        .unwrap_or(false),
                    _ => false,
                };
                if !creates {
                    return Err(p57_no_such_file(ctx, &p));
                }
            }
            // Real file: return a working `FileChannel` (which implements
            // `SeekableByteChannel`) so `read`/`write`/`position`/`size`/`close`
            // resolve to the fd_table-backed `FileChannel` natives. Previously
            // this returned a synthetic object allocated AS the bare
            // `SeekableByteChannel` interface with no method bodies, so
            // `channel.write(buf)` / `channel.read(buf)` dispatched to the
            // abstract interface method → `AbstractMethodError:
            // WritableByteChannel.write … has no Code attribute`
            // (SC-resource-io-family Cause A). Delegate to the sibling
            // `newFileChannel` shim, which parses the `OpenOption` Set
            // (READ/WRITE/APPEND/CREATE/TRUNCATE_EXISTING) and opens the fd.
            // NB: inline the class-name literal (not the `fsp` binding) so this
            // closure stays non-capturing — `r.register` takes a bare `fn`.
            ctx.invoke(
                "java/nio/file/spi/FileSystemProvider",
                "newFileChannel",
                "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;",
                args,
            )
        },
    );

    r.register(
        fsp,
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
        // args[0] = this (the provider), args[1] = the Path.
        |ctx, args| fsp_new_input_stream(ctx, args, 1),
    );

    // FileSystemProvider.newOutputStream — JDK's default impl at
    // `FileSystemProvider.java:426` calls `newByteChannel(path, opts)` which
    // our minimal `newByteChannel` registration handles as READ-ONLY via
    // `std::fs::read(&p)`. For a NEW file (the whole point of an output
    // stream) `std::fs::read` ENOENTs, the channel allocation fails, and the
    // caller sees `IOException: file not found`. This breaks any code that
    // uses `Files.newOutputStream` / `Files.write(Path, byte[])` to *create*
    // a file — most prominently Felix's `BundleArchive.writeBundleInfo`,
    // which is invoked once per installed bundle. Without it the entire
    // Felix auto-deploy directory fails to install (every `installBundle`
    // throws `BundleException: Unable to cache bundle`), no Gogo shell
    // bundles activate, no shell-reader thread spawns, and `felix.jar` hangs
    // forever in `Felix.waitForStop(0)` because nothing ever calls
    // `framework.stop()`.
    //
    // The fix: register `newOutputStream` directly so we never fall through
    // to the JDK default. We open the path for writing through `fd_table`
    // (honouring APPEND/CREATE/etc. via `open_options_*` helpers below) and
    // hand back a real `java.io.FileOutputStream` instance with the fd
    // wired onto its `FileDescriptor` — that way every downstream
    // `write`/`flush`/`close` native (already registered against
    // `java/io/FileOutputStream`) just works without any new shim type.
    r.register(
        fsp,
        "newOutputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/OutputStream;",
        |ctx, args| fsp_new_output_stream(ctx, args),
    );

    r.register(
        fsp,
        "checkAccess",
        "(Ljava/nio/file/Path;[Ljava/nio/file/AccessMode;)V",
        |ctx, args| {
            let path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, path_obj);
            let exists = match vfs_classify(&p) {
                Some(kind) => !matches!(kind, JarFsKind::Absent),
                None => std::path::Path::new(&p).exists(),
            };
            if !exists {
                return Err(RuntimeError::IOException {
                    message: format!("NoSuchFileException: {}", p),
                }
                .into());
            }
            Ok(None)
        },
    );

    r.register(
        fsp,
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;",
        |ctx, args| {
            #[cfg(windows)]
            {
                // See the `Files.readAttributes` registration: only a PRESENT,
                // NAMEABLE `Class` argument states a requested type. Absent or
                // unresolvable means "no request" and falls back to the
                // declared return type, `BasicFileAttributes`.
                if let Some(Value::Object(Some(class))) = args.get(2) {
                    let requested_type =
                        crate::lang_class::mirror_class_name(ctx, *class).unwrap_or_default();
                    if !requested_type.is_empty()
                        && !windows_supports_file_attributes_type(&requested_type)
                    {
                        return Err(RuntimeError::UnsupportedOperationException {
                            message: format!(
                                "File attribute type {requested_type} is not supported on Windows"
                            ),
                        }
                        .into());
                    }
                }
            }
            let path_obj = obj_arg(args, 1)?;
            let options = args.get(3).copied().unwrap_or(Value::Object(None));
            p59_files_read_attributes(ctx, &[Value::Object(Some(path_obj)), options])
        },
    );

    // `FileSystemProvider.readAttributes(Path, String, LinkOption...)` — the
    // name-keyed sibling of the `Class`-keyed form above.
    //
    // In the real JDK this is implemented on `sun.nio.fs.AbstractFileSystemProvider`
    // and every concrete provider inherits it; `java.nio.file.spi.FileSystemProvider`
    // itself only declares it, abstract. CratonVM's default-filesystem provider
    // object is stamped with that abstract class (`FileSystems.getDefault()
    // .provider()` reports a concrete name only through `getClass()` display
    // remapping), so real-JDK bytecode calling this method resolved the abstract
    // declaration and died with
    // `AbstractMethodError: ... readAttributes ... has no Code attribute`.
    //
    // That is not a corner: `Files.readAttributes(path, "basic:*")`,
    // `Files.getAttribute`, and everything layered on them route here. It is
    // also why `com.sun.tools.attach.VirtualMachine.list()` threw
    // `InternalError` — jvmstat's `PlatformSupportImpl` reads `unix:dev` on the
    // temp directory during container detection, and the `AbstractMethodError`
    // came back out wrapped two deep.
    //
    // Args: `[this, path, attributes, options]`.
    r.register(
        fsp,
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/util/Map;",
        |ctx, args| {
            let path_obj = obj_arg(args, 1)?;
            let spec = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let nofollow =
                matches!(args.get(3), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
            read_named_attributes(ctx, path_obj, &spec, nofollow)
        },
    );

    // `FileSystemProvider.setAttribute(Path, String, Object, LinkOption...)` —
    // the write-side twin of the reader above, missing for exactly the same
    // reason and left behind when that one was added.
    //
    // `Files.setAttribute` is how everything name-keyed writes: H2's
    // `FilePathDisk.setReadOnly` takes this branch whenever the FileStore
    // reports DOS rather than POSIX attributes (i.e. on Windows), which made
    // `org.h2.test.unit.TestFileSystem` die at `testSetReadOnly` on EVERY
    // filesystem prefix it exercises. It is not a Windows defect: the Linux
    // witness fails identically, H2 just reaches
    // `Files.setPosixFilePermissions` there instead.
    //
    // Args: `[this, path, attribute, value, options]`.
    r.register(
        fsp,
        "setAttribute",
        "(Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V",
        |ctx, args| {
            let path_obj = obj_arg(args, 1)?;
            let spec = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let value = match args.get(3) {
                Some(Value::Object(v)) => *v,
                _ => None,
            };
            let nofollow =
                matches!(args.get(4), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
            write_named_attribute(ctx, path_obj, &spec, value, nofollow)
        },
    );

    r.register(
        fsp,
        "newDirectoryStream",
        "(Ljava/nio/file/Path;Ljava/nio/file/DirectoryStream$Filter;)Ljava/nio/file/DirectoryStream;",
        |ctx, args| {
            let path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, path_obj);
            // 2 fields: slot 0 = materialised Object[] of Paths, slot 1 = the
            // closed flag `close()` sets (see the `close`/`iterator`
            // registrations below).
            let stream = alloc_concurrent_synthetic(ctx, "java/nio/file/DirectoryStream", 2);
            // Pin across the array/Path allocs below — a moving young GC there
            // would relocate the fresh stream/array (native stale-local family).
            let stream_pin = ctx.pin_native_root(stream);
            // Build array of Path entries
            let entries: Vec<String> = if let Some((jar, dir)) = jarfs_decode(&p) {
                jarfs_list_dir(&jar, &dir)
                    .into_iter()
                    .map(|child| jarfs_encode(&jar, &child))
                    .collect()
            } else if let Some((java_home, dir)) = jrtfs_decode(&p) {
                jrtfs_list_dir_classified(&java_home, &dir)
                    .into_iter()
                    .map(|(child, _)| jrtfs_encode(&java_home, &child))
                    .collect()
            } else {
                match std::fs::read_dir(&p) {
                    Ok(rd) => rd
                        .filter_map(|e| e.ok())
                        .map(|e| e.path().to_string_lossy().replace('\\', "/"))
                        .collect(),
                    Err(_) => vec![],
                }
            };
            use cratonvm_types::ArrayElementType;
            let arr = ctx.new_array(ArrayElementType::Reference, entries.len());
            let arr_pin = ctx.pin_native_root(arr);
            for (i, entry) in entries.iter().enumerate() {
                let ep = p57_alloc_path(ctx, entry);
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(ep)));
            }
            let stream = ctx.read_native_pin(stream_pin, stream);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            ctx.unpin_native_roots(stream_pin);
            Ok(Some(Value::Object(Some(stream))))
        },
    );

    // DirectoryStream is an interface — iterator()/close() are abstract on
    // it. Jetty start.jar (BaseHome.<init> → JettyBaseConfigSource) and
    // Spring Boot's path scanning both walk newDirectoryStream(...) via
    // for-each, which compiles to invokeinterface DirectoryStream.iterator.
    // Without these natives every such walk throws
    // `AbstractMethodError: DirectoryStream.iterator()V has no Code attribute`.
    let ds = "java/nio/file/DirectoryStream";
    r.register(ds, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Spec: iterating a closed stream is a `ClosedDirectoryStreamException`,
        // which IS an `IllegalStateException` (that is its declared supertype),
        // so this is the same class of failure real `UnixDirectoryStream`
        // raises — not a substitute for it.
        if matches!(ctx.get_field(this, 1), Value::Int(v) if v != 0) {
            return Err(RuntimeError::IllegalStateException {
                message: "directory stream is closed".to_string(),
            }
            .into());
        }
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
        };
        let len = ctx.array_length(arr);
        cratonvm_native_collections::make_iterator_from_array(ctx, arr, len)
    });
    // Was an unconditional no-op, justified by "this stream owns no OS
    // directory handle". It owns something else that `close()` is contracted
    // to release: `newDirectoryStream` above materialises the ENTIRE listing
    // into a Java Object[] held in slot 0, which a closed stream must not keep
    // alive (a walk over a large tree pinned every directory's listing for as
    // long as the stream object was reachable) and must not keep serving.
    // Drop the array and latch the closed flag; `close()` stays idempotent,
    // as the spec requires.
    r.register(ds, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None));
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });
    // Was `null`, which is not a legal answer for anything: `DirectoryStream`
    // extends `Iterable`, whose `spliterator()` default body returns
    // `Spliterators.spliteratorUnknownSize(iterator(), 0)` — never null — so
    // `StreamSupport.stream(ds.spliterator(), false)` (and every for-each that
    // the compiler routes through it) NPE'd instead of "falling back to
    // iterator()". `newDirectoryStream` already materialised the whole listing
    // into the Object[] in slot 0, so hand back the same 3-field
    // (array, pos, fence) synthetic Spliterator the `Spliterator.*` natives and
    // `Spliterators.spliteratorUnknownSize` already build.
    r.register(
        ds,
        "spliterator",
        "()Ljava/util/Spliterator;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
            };
            let len = ctx.array_length(arr) as i32;
            // Pin across the allocation below — a moving young GC there would
            // relocate the listing array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            let spl = alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(spl, 0, Value::Object(Some(arr)));
            ctx.set_field(spl, 1, Value::Int(0));
            ctx.set_field(spl, 2, Value::Int(len));
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(spl))))
        },
    );

    // createSymbolicLink / createLink / readSymbolicLink.
    //
    // `Files.createSymbolicLink(link, target, attrs)` is ordinary (non-native)
    // JDK bytecode: `provider(link).createSymbolicLink(link, target, attrs);
    // return link;`. The default provider this VM hands back is the synthetic
    // instance stamped as the literal `java/nio/file/spi/FileSystemProvider`
    // class (see the `getFileStore` note above), so the invokevirtual lands on
    // `FileSystemProvider`'s OWN concrete body — which, on the abstract base,
    // is an unconditional `throw new UnsupportedOperationException()`. That is
    // exactly what
    // docs/known-issues/springboot/files-createsymboliclink-unsupported-20260731.md
    // reported: every `Files.createSymbolicLink` call in the VM died with a
    // bare `UnsupportedOperationException` at `FileSystemProvider.java:626`,
    // taking out `ConfigTreePropertySourceTests` (Kubernetes ConfigMap
    // `..data`-symlink shapes) and `FileWatcherTests` (symlink-following
    // watch registration).
    //
    // Registering the three link methods HERE, on `fsp`, is what makes them
    // reachable: a registration on `java/nio/file/Files` (there were three,
    // now real, further down this file) never runs for these calls because
    // `Files.createSymbolicLink` has real bytecode of its own and this VM
    // prefers concrete bytecode over a bridge registration.
    r.register(
        fsp,
        "createSymbolicLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
        |ctx, args| {
            let link = obj_arg(args, 1)?;
            let target = obj_arg(args, 2)?;
            let link = p57_read_path(ctx, link);
            let target = p57_read_path(ctx, target);
            p57_create_symbolic_link(ctx, &link, &target)?;
            Ok(None)
        },
    );
    r.register(
        fsp,
        "createLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)V",
        |ctx, args| {
            let link = obj_arg(args, 1)?;
            let existing = obj_arg(args, 2)?;
            let link = p57_read_path(ctx, link);
            let existing = p57_read_path(ctx, existing);
            p57_create_hard_link(ctx, &link, &existing)?;
            Ok(None)
        },
    );
    r.register(
        fsp,
        "readSymbolicLink",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, path_obj);
            p57_read_symbolic_link(ctx, &p)
        },
    );

    // newFileChannel(Path, Set<? extends OpenOption>, FileAttribute[]) -> FileChannel
    // Real JDK delegates to WindowsFileSystemProvider.newFileChannel which
    // overrides this abstract method. We provide a synthetic FileChannel
    // backed by fd_table (same shape as RandomAccessFile.getChannel) so the
    // existing j.n.c.FileChannel native methods can drive it.
    r.register(
        fsp,
        "newFileChannel",
        "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            let path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, path_obj);
            // Inspect option set: look for WRITE/APPEND/CREATE/CREATE_NEW/READ/
            // TRUNCATE_EXISTING via toString.
            let set_obj = match args.get(2) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            let (mut writable, mut create, mut append, mut read_opt, mut truncate) =
                (false, false, false, false, false);
            if let Some(set) = set_obj {
                // Try to iterate by calling toString() on the Set first (cheap & robust)
                if let Ok(Some(Value::Object(Some(s)))) =
                    ctx.invoke_virtual(set, "toString", "()Ljava/lang/String;", &[])
                {
                    let s = ctx.read_string(s).unwrap_or_default();
                    writable = s.contains("WRITE") || s.contains("APPEND");
                    create = s.contains("CREATE"); // matches CREATE and CREATE_NEW
                    append = s.contains("APPEND");
                    read_opt = s.contains("READ");
                    truncate = s.contains("TRUNCATE_EXISTING");
                }
            }
            // JDK FileChannel.open contract: a channel with neither READ nor
            // WRITE is read-only; WRITE without READ is write-only.
            let readable = read_opt || !writable;
            // NOFOLLOW_LINKS: refuse a symlink final component before the open.
            // This registration is the funnel for `FileChannel.open`, for
            // `Files.newByteChannel` (which delegates here) and for the JDK's
            // own `newOutputStream`/`newInputStream` defaults, so one check here
            // covers all four. `set_obj`'s toString already told us the answer.
            if fsp_scan_open_options(ctx, set_obj.map(|o| Value::Object(Some(o)))).nofollow {
                if let Some(refused) = p57_nofollow_reject(&p) {
                    return Err(refused);
                }
            }
            // Real `FileChannel.open` throws `java.nio.file.NoSuchFileException`
            // (not a bare `IOException`) when the target is missing — callers
            // like `FileSystemResource.readableChannel()` explicitly catch
            // `NoSuchFileException` and translate it to `FileNotFoundException`
            // (ResourceTests#resourceCreateRelativeUnknown). Mapping every open
            // failure to a generic `IOException` made that catch miss, so the
            // raw `IOException` propagated to the caller instead.
            // GAP I2: these openers used to call `fd_table()` directly, so the
            // whole `java.nio.file` surface bypassed every path policy in the
            // VM. Route through the capability gate, which runs the check
            // before the fd is reserved and before the syscall. With no policy
            // installed (today's default) this is the same call as before.
            let gated = if writable {
                crate::capability_gate::open_read_write_gated(&*ctx, &p, create)
            } else {
                // Read-only request: try read+write first (a seekable fd), and
                // fall back to a plain read. A `FileWrite` denial takes the
                // same fallback an `EACCES` would, which is the right answer —
                // the caller only asked to read.
                match crate::capability_gate::open_read_write_gated(&*ctx, &p, false) {
                    Ok(fd) => Ok(fd),
                    Err(_) => crate::capability_gate::open_read_gated(&*ctx, &p),
                }
            };
            let fd_id = match gated {
                Ok(fd) => fd,
                // A refusal is a `SecurityException`. It must NOT become
                // `NoSuchFileException`: callers such as
                // `FileSystemResource.readableChannel()` catch that one and
                // recover, which would silently swallow the policy decision.
                Err(cratonvm_native_api::fd_table::FdCapabilityError::Denied(denied)) => {
                    return Err(denied.into())
                }
                Err(cratonvm_native_api::fd_table::FdCapabilityError::Io(e)) => {
                    return Err(if e.kind() == std::io::ErrorKind::NotFound {
                        RuntimeError::NoSuchFileException { path: p.clone() }.into()
                    } else {
                        MethodCallFailed::from(RuntimeError::IOException {
                            message: format!("Cannot open {}: {}", p, e),
                        })
                    })
                }
            };
            if truncate && writable {
                let _ = ctx.fd_table().rw_set_length(fd_id, 0);
            }
            if append {
                if let Ok(sz) = ctx.fd_table().file_size(fd_id) {
                    let _ = ctx.fd_table().rw_seek(fd_id, std::io::SeekFrom::Start(sz));
                }
            }

            // RECONCILE-WITH-REAL: build a REAL `sun.nio.ch.FileChannelImpl`
            // over the fd (FileDescriptor.handle = fd_table id, exactly how
            // FileInputStream/FileOutputStream back their channels) and return
            // it. `read/write/size/position/truncate` then resolve to
            // FileChannelImpl's CONCRETE bytecode → the working
            // IOUtil→FileDispatcherImpl native path (which now routes the temp
            // direct-buffer arena handle correctly). Native dispatch is keyed
            // on the resolved method's declaring class, so a concrete-class
            // receiver never hits the abstract-`FileChannel` synthetic shims —
            // unlike the legacy synthetic channel below, whose shims assume a
            // synthetic ByteBuffer layout and corrupt real heap buffers.
            //
            // FileChannelImpl.open(fd, path, readable, writable, sync, direct,
            //   parent) — mirrors FileOutputStream.getChannel's call shape.
            let real_channel = (|| -> Option<Value> {
                let fd_obj = match ctx.new_object("java/io/FileDescriptor").ok()?? {
                    Value::Object(Some(o)) => o,
                    _ => return None,
                };
                // `handle` is the Windows fd slot fd_from_descriptor prefers;
                // also set `fd` for the POSIX read path.
                ctx.set_field_by_name(fd_obj, "handle", Value::Long(fd_id as i64));
                ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd_id as i32));
                let path_str = ctx.create_string(&p);
                ctx.ensure_class_initialized("sun/nio/ch/FileChannelImpl").ok()?;
                match ctx.invoke(
                    "sun/nio/ch/FileChannelImpl",
                    "open",
                    "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;",
                    &[
                        Value::Object(Some(fd_obj)),
                        Value::Object(Some(path_str)),
                        Value::Int(readable as i32),
                        Value::Int(writable as i32),
                        Value::Int(0), // sync
                        Value::Int(0), // direct
                        Value::Object(None), // parent
                    ],
                ) {
                    Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
                    _ => None,
                }
            })();
            if let Some(v) = real_channel {
                return Ok(Some(v));
            }

            // Legacy synthetic fallback (only if the real FileChannelImpl
            // construction is unavailable — keeps prior behavior intact).
            let fc = alloc_concurrent_synthetic(ctx, "java/nio/channels/FileChannel", 1);
            ctx.set_field(fc, 0, Value::Int(fd_id as i32));
            Ok(Some(Value::Object(Some(fc))))
        },
    );

    // Round 63 — Kafka 4.2.0 calls `FileChannel.size()` on the synthetic
    // FileChannel returned by `newFileChannel` above (via
    // `BatchFileReader.build → FileRecords.<init>`). The real JDK
    // FileChannel declares `size()` abstract — without a native here we
    // throw AbstractMethodError and Kafka exits silently through its
    // outer `Throwable` catch + `Exit.exit(1)`. Register a minimal set
    // of FileChannel natives that drive the synthetic via fd_table.
    let fc_cls = "java/nio/channels/FileChannel";
    r.register(fc_cls, "size", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = match ctx.get_field(this, 0) {
            Value::Int(v) if v >= 0 => v as u32,
            _ => return Ok(Some(Value::Long(0))),
        };
        let sz = ctx.fd_table().file_size(fd_id).unwrap_or(0);
        Ok(Some(Value::Long(sz as i64)))
    });
    r.register(fc_cls, "position", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = match ctx.get_field(this, 0) {
            Value::Int(v) if v >= 0 => v as u32,
            _ => return Ok(Some(Value::Long(0))),
        };
        let pos = ctx
            .fd_table()
            .rw_seek(fd_id, std::io::SeekFrom::Current(0))
            .unwrap_or(0);
        Ok(Some(Value::Long(pos as i64)))
    });
    r.register(
        fc_cls,
        "position",
        "(J)Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let fd_id = match ctx.get_field(this, 0) {
                Value::Int(v) if v >= 0 => v as u32,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let _ = ctx
                .fd_table()
                .rw_seek(fd_id, std::io::SeekFrom::Start(pos as u64));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(fc_cls, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // See docs/known-issues/h2/!bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md:
        // this native is registered on the literal "java/nio/channels/FileChannel"
        // class to service a synthetic single-field FileChannel, but native
        // overrides shadow ALL dispatch for that class name -- including a real
        // `sun/nio/ch/FileChannelImpl` reaching an inherited method (close()V is
        // declared in the grandparent AbstractInterruptibleChannel, not
        // FileChannel itself) through a FileChannel-typed call site. Detect a
        // real instance and replicate AbstractInterruptibleChannel.close()'s
        // contract by calling the real implCloseChannel() bytecode instead of
        // treating field 0 as a synthetic fd.
        let class_name = ctx.class_name_of_id(ctx.class_id_of_object(this));
        if class_name.as_deref() != Some("java/nio/channels/FileChannel") {
            if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
                return Ok(None);
            }
            ctx.set_field_by_name(this, "closed", Value::Int(1));
            ctx.invoke_virtual(this, "implCloseChannel", "()V", &[])?;
            return Ok(None);
        }
        if let Value::Int(v) = ctx.get_field(this, 0) {
            if v >= 0 {
                let _ = ctx.fd_table().close(v as u32);
            }
        }
        Ok(None)
    });
    r.register(fc_cls, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let class_name = ctx.class_name_of_id(ctx.class_id_of_object(this));
        if class_name.as_deref() != Some("java/nio/channels/FileChannel") {
            return Ok(Some(Value::Int(
                if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
                    0
                } else {
                    1
                },
            )));
        }
        Ok(Some(Value::Int(1)))
    });
    // write(ByteBuffer)I — `FileChannel.write` is abstract; cassandra's
    // BufferedDataOutputStreamPlus.doFlush drives a synthetic FileChannel
    // (from `newFileChannel`) through it. ByteBuffer layout:
    // field 0 = backing array, 1 = position, 2 = limit.
    r.register(fc_cls, "write", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = match ctx.get_field(this, 0) {
            Value::Int(v) if v >= 0 => v as u32,
            _ => {
                return Err(RuntimeError::IOException {
                    message: "Channel closed".into(),
                }
                .into())
            }
        };
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Int(0))),
        };
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut data = vec![0u8; remaining];
        if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
            for (i, d) in data.iter_mut().enumerate() {
                *d = ctx.get_array_element(arr, bb_pos + i).as_int().unwrap_or(0) as u8;
            }
        }
        let n = ctx
            .fd_table()
            .rw_write(fd_id, &data)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
        Ok(Some(Value::Int(n as i32)))
    });
    // read(ByteBuffer)I — counterpart of write above.
    r.register(fc_cls, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = match ctx.get_field(this, 0) {
            Value::Int(v) if v >= 0 => v as u32,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut buf = vec![0u8; remaining];
        match ctx.fd_table().rw_read(fd_id, &mut buf) {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                    for (i, b) in buf.iter().take(n).enumerate() {
                        ctx.set_array_element(arr, bb_pos + i, Value::Int(*b as i8 as i32));
                    }
                }
                ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
                Ok(Some(Value::Int(n as i32)))
            }
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    });

    // --- Files additional methods ---
    r.register(
        files,
        "exists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| Ok(Some(Value::Int(i32::from(p57_files_exists_impl(ctx, args))))),
    );

    r.register(
        files,
        "notExists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| Ok(Some(Value::Int(i32::from(!p57_files_exists_impl(ctx, args))))),
    );

    r.register(
        files,
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
        // args[0] = the Path (static method).
        |ctx, args| fsp_new_input_stream(ctx, args, 0),
    );

    r.register(
        files,
        "newBufferedReader",
        "(Ljava/nio/file/Path;Ljava/nio/charset/Charset;)Ljava/io/BufferedReader;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            match p57_read_to_string(&p) {
                Ok(content) => files_make_buffered_reader_over_string(ctx, &content),
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );

    r.register(
        files,
        "newBufferedReader",
        "(Ljava/nio/file/Path;)Ljava/io/BufferedReader;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            match p57_read_to_string(&p) {
                Ok(content) => files_make_buffered_reader_over_string(ctx, &content),
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );

    // RWF86.1 (gated): native shims for BufferedReader.read([CII)I and read()I.
    //
    // These delegate `read` straight to the Reader at slot 0, BYPASSING
    // java.io.BufferedReader's own buffer (`cb`/`nChars`/`nextChar`) and its
    // mark()/reset() bookkeeping. That is only correct for the *synthetic*
    // BufferedReader layout (slot 0 = wrapped Reader, no real JDK fields), which
    // only exists under the `synthetic-jdk` feature. In the default real-JDK
    // build every BufferedReader is a genuine JDK instance, and shadowing its
    // `read` with this passthrough silently broke mark()/reset(): after a
    // `mark(); read(); reset()` the chars consumed by the native read were lost
    // because the real reset() rewinds buffer indices the native never advanced.
    // H2's RUNSCRIPT/CSV/INIT BOM-skip (`mark(1); read(); reset()`) dropped the
    // first character of every script — `"create table"` parsed as `"reate
    // table"`. `Files.newBufferedReader` now builds a real BufferedReader, so
    // the side-table is never populated and these shims serve no real-JDK use.
    #[cfg(feature = "synthetic-jdk")]
    {
        r.register("java/io/BufferedReader", "read", "([CII)I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let out_arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let off = match args.get(2) {
                Some(Value::Int(v)) => *v as usize,
                _ => 0,
            };
            let len = match args.get(3) {
                Some(Value::Int(v)) => *v as usize,
                _ => 0,
            };
            if len == 0 {
                return Ok(Some(Value::Int(0)));
            }
            let out_len = ctx.array_length(out_arr);
            if off > out_len || off.saturating_add(len) > out_len {
                return Ok(Some(Value::Int(-1)));
            }
            // Side-table-backed reader (from Files.newBufferedReader).
            if let Some(n) = br_sidetable_read_chars(ctx, this, out_arr, off, len) {
                return Ok(Some(Value::Int(n)));
            }
            // Fallback: delegate to underlying Reader at slot 0.
            match ctx.get_field(this, 0) {
                Value::Object(Some(inner)) => {
                    let r = ctx.invoke_virtual(
                        inner,
                        "read",
                        "([CII)I",
                        &[
                            Value::Object(Some(out_arr)),
                            Value::Int(off as i32),
                            Value::Int(len as i32),
                        ],
                    )?;
                    Ok(r.or(Some(Value::Int(-1))))
                }
                _ => Ok(Some(Value::Int(-1))),
            }
        });
        r.register("java/io/BufferedReader", "read", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(c) = br_sidetable_read_one(ctx, this) {
                return Ok(Some(Value::Int(c)));
            }
            match ctx.get_field(this, 0) {
                Value::Object(Some(inner)) => {
                    let r = ctx.invoke_virtual(inner, "read", "()I", &[])?;
                    Ok(r.or(Some(Value::Int(-1))))
                }
                _ => Ok(Some(Value::Int(-1))),
            }
        });
    } // end #[cfg(feature = "synthetic-jdk")] BufferedReader.read shims

    // (The APPEND-only OpenOption[] scanner that used to live here is gone:
    // `fsp_scan_open_options` reads the same array and also reports CREATE_NEW
    // and NOFOLLOW_LINKS, which this path has to honour too.)

    // DELETED 2026-08-05: the fd-backed `Files.newBufferedWriter` that used to
    // live here, and the `CRATONVM_SYNTHETIC_BUFFERED_WRITER` flag that gated
    // it back on.
    //
    // It allocated a `java/io/BufferedWriter`, wrote the OS file descriptor
    // into raw slot 0 — `java.io.Writer.writeBuffer` on the real layout, a
    // `char[]` the JDK owns — and left the six BufferedWriter natives below to
    // recognise their own object by asking whether that slot held an `Int`.
    // That is kind 3 in
    // `docs/known-issues/jdk-only/fabricated-object-layouts-leak-into-native-code.md`:
    // a VM value with no real field to live in.
    //
    // It was already default-OFF (real bytecode has been the default since
    // 2026-06-18, because this path "silently DROPPED all character data"), and
    // measuring the flagged arm before touching it showed it had no working
    // configuration left at all: `probes/BufferedWriterDiscriminatorProbe`
    // produced ZERO bytes for every one of its five `newBufferedWriter` writes,
    // and the overlay trace showed the fd being written once
    // (`set_field slot=0 value=Int(3)`) and every subsequent read of that slot
    // on the same object returning `Object(None)` — the fd destroyed before its
    // first use.
    //
    // So it is deleted rather than repaired or relocated to a side table: it is
    // the only writer of that overlay in a default build, and the default path
    // (real `BufferedWriter(OutputStreamWriter(Files.newOutputStream(p)))`) is
    // byte-identical to HotSpot across every line of that probe.

    // Minimal BufferedWriter natives backed by the fd stored at slot 0.
    // These are also registered in synthetic-jdk mode by phases_late, but
    // the registry dedups on (class, name, desc) so re-registering is
    // safe and keeps the contract explicit for Files.newBufferedWriter.
    //
    // These natives are registered on the REAL `java/io/BufferedWriter`
    // class, so in real-JDK mode they shadow EVERY BufferedWriter — not
    // just the synthetic fd-backed object that `Files.newBufferedWriter`
    // returns. A real `new BufferedWriter(new OutputStreamWriter(System.out))`
    // (the picocli / JUnit-console help-text writer) stores the wrapped
    // `Writer` in slot 0, not an `Int` fd, so the old `_ => Ok(None)` arms
    // silently DROPPED its output → empty `--help`. `bw_delegate_out`
    // distinguishes the two: when slot 0 is not an `Int`, the object is a
    // real BufferedWriter and the native forwards to its wrapped `out`
    // Writer so the genuine OutputStreamWriter/StreamEncoder bytecode runs.
    let bw_class = "java/io/BufferedWriter";
    r.register(bw_class, "write", "(Ljava/lang/String;II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(out) = bw_delegate_out(ctx, this) {
            let s = args.get(1).cloned().unwrap_or(Value::Object(None));
            let off = args.get(2).cloned().unwrap_or(Value::Int(0));
            let len = args.get(3).cloned().unwrap_or(Value::Int(0));
            let _ = ctx.invoke_virtual(out, "write", "(Ljava/lang/String;II)V", &[s, off, len]);
            return Ok(None);
        }
        let text = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or_default() as usize;
        let fd = match ctx.get_field(this, 0) {
            Value::Int(fd) => fd as u32,
            _ => return Ok(None),
        };
        let end = off.saturating_add(len).min(text.chars().count());
        let sub: String = text
            .chars()
            .skip(off)
            .take(end.saturating_sub(off))
            .collect();
        let _ = ctx.fd_table().write_string(fd, &sub);
        Ok(None)
    });
    r.register(bw_class, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(out) = bw_delegate_out(ctx, this) {
            let c = args.get(1).cloned().unwrap_or(Value::Int(0));
            let _ = ctx.invoke_virtual(out, "write", "(I)V", &[c]);
            return Ok(None);
        }
        let c = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u32;
        let fd = match ctx.get_field(this, 0) {
            Value::Int(fd) => fd as u32,
            _ => return Ok(None),
        };
        if let Some(ch) = char::from_u32(c) {
            let mut buf = [0u8; 4];
            let bytes = ch.encode_utf8(&mut buf).as_bytes();
            let _ = ctx.fd_table().write_bytes(fd, bytes);
        }
        Ok(None)
    });
    r.register(bw_class, "write", "([CII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(out) = bw_delegate_out(ctx, this) {
            let arr = args.get(1).cloned().unwrap_or(Value::Object(None));
            let off = args.get(2).cloned().unwrap_or(Value::Int(0));
            let len = args.get(3).cloned().unwrap_or(Value::Int(0));
            let _ = ctx.invoke_virtual(out, "write", "([CII)V", &[arr, off, len]);
            return Ok(None);
        }
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(None),
        };
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let fd = match ctx.get_field(this, 0) {
            Value::Int(fd) => fd as u32,
            _ => return Ok(None),
        };
        let cap = ctx.array_length(arr);
        let end = off.saturating_add(len).min(cap);
        let mut chars: Vec<u16> = Vec::with_capacity(end.saturating_sub(off));
        for i in off..end {
            if let Value::Int(v) = ctx.get_array_element(arr, i) {
                chars.push((v & 0xFFFF) as u16);
            }
        }
        let s = String::from_utf16_lossy(&chars);
        let _ = ctx.fd_table().write_string(fd, &s);
        Ok(None)
    });
    r.register(bw_class, "newLine", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sep = ctx
            .get_system_property("line.separator")
            .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string());
        if let Some(out) = bw_delegate_out(ctx, this) {
            let s = ctx.create_string(&sep);
            let _ = ctx.invoke_virtual(
                out,
                "write",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(s))],
            );
            return Ok(None);
        }
        let fd = match ctx.get_field(this, 0) {
            Value::Int(fd) => fd as u32,
            _ => return Ok(None),
        };
        let _ = ctx.fd_table().write_string(fd, &sep);
        Ok(None)
    });
    r.register(bw_class, "flush", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(out) = bw_delegate_out(ctx, this) {
            let _ = ctx.invoke_virtual(out, "flush", "()V", &[]);
            return Ok(None);
        }
        // BufWriter<File> flushes automatically on drop; explicit
        // flush is a no-op in the direct-fd mode since each write
        // already hits the buffered writer inside fd_table.
        Ok(None)
    });
    r.register(bw_class, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(out) = bw_delegate_out(ctx, this) {
            let _ = ctx.invoke_virtual(out, "flush", "()V", &[]);
            let _ = ctx.invoke_virtual(out, "close", "()V", &[]);
            return Ok(None);
        }
        let fd = match ctx.get_field(this, 0) {
            Value::Int(fd) => fd as u32,
            _ => return Ok(None),
        };
        let _ = ctx.fd_table().close(fd);
        ctx.set_field(this, 0, Value::Int(-1));
        Ok(None)
    });

    r.register(
        files,
        "readAllBytes",
        "(Ljava/nio/file/Path;)[B",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            let read = match vfs_read(&p) {
                Some(r) => r,
                None => std::fs::read(&p),
            };
            match read {
                Ok(data) => {
                    use cratonvm_types::ArrayElementType;
                    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
                    ctx.write_byte_array_from(arr, 0, &data);
                    Ok(Some(Value::Object(Some(arr))))
                }
                // NIO contract: missing file → NoSuchFileException (see
                // newByteChannel above), not a bare IOException — callers like
                // FileSystemResource.getContentAsByteArray() catch
                // NoSuchFileException and translate it to FileNotFoundException
                // (ResourceTests#resourceCreateRelativeUnknown).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p))
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );

    // --- Path additional methods ---
    let path = "java/nio/file/Path";
    r.register(
        path,
        "toAbsolutePath",
        "()Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, this);
            // JDK `Path.toAbsolutePath()` does NOT resolve symlinks or require the
            // file to exist (that is `toRealPath`) — it only makes a *relative*
            // path absolute. Must NOT `canonicalize()` here: this build runs UNIX-
            // mode `std::path` on Windows, so `canonicalize` on an existing path
            // returns a `\\?\C:\…` verbatim path that renders as `//?/C:/…` and
            // breaks downstream string-based Path ops — e.g. WildFly's
            // `Environment.validateWildFlyDir` then rejects a valid `jboss.dist`
            // ("could not find jboss-modules.jar" though it exists). A drive-letter
            // (`C:`), UNC (`\\`/`//`), or leading-separator path is already absolute
            // and returned unchanged; a relative path is anchored to the CWD.
            let abs = p57_absolute_path_string(&p);
            let result = p57_alloc_path(ctx, &abs);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // `Path.toRealPath(LinkOption...)` — resolve to the real, canonical path of
    // an *existing* file. Per the JDK contract this method throws
    // `java.nio.file.NoSuchFileException` when the file does not exist; callers
    // such as Jetty's `start.jar` (`DirConfigSource.<init>`) rely on that
    // specific exception type — its bytecode has an exception-table entry that
    // catches `NoSuchFileException` to fall through to `start.d` scanning.
    // Previously `toRealPath` was unregistered and the real-JDK `WindowsPath`
    // bytecode (whose `WindowsNativeDispatcher` natives are not implemented)
    // returned `null`, so `FS.canReadFile(null)` -> `Files.exists(null)` raised
    // an uncatchable NPE. We must return a non-null Path or throw the *typed*
    // `NoSuchFileException` so the catch clause matches.
    r.register(
        path,
        "toRealPath",
        "([Ljava/nio/file/LinkOption;)Ljava/nio/file/Path;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, this);
            // jar-filesystem entries: a real path inside a mounted jar. If the
            // entry is present, the path itself is already "real"; otherwise
            // it does not exist.
            if vfs_decode(&p).is_some() {
                if matches!(vfs_classify(&p), Some(JarFsKind::Absent) | None) {
                    return Err(p57_no_such_file(ctx, &p));
                }
                let result = p57_alloc_path(ctx, &p);
                return Ok(Some(Value::Object(Some(result))));
            }
            match std::fs::canonicalize(&p) {
                Ok(c) => {
                    let real = c.to_string_lossy().replace('\\', "/");
                    // Strip the Windows `\\?\` extended-length prefix that
                    // `canonicalize` adds, so the path stays usable by other
                    // string-based Path natives.
                    let real = real
                        .strip_prefix("//?/")
                        .map(|s| s.to_string())
                        .unwrap_or(real);
                    let result = p57_alloc_path(ctx, &real);
                    Ok(Some(Value::Object(Some(result))))
                }
                Err(_) => Err(p57_no_such_file(ctx, &p)),
            }
        },
    );

    // Route through the shared root-aware `p57_normalize_path` (keeps the root
    // and leading `..` on relative paths) instead of an inline `..`-pop that
    // dropped the drive root. This is the last-registered (winning) `normalize`.
    r.register(path, "normalize", "()Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let normalized = p57_normalize_path(&p);
        let result = p57_alloc_path(ctx, &normalized);
        Ok(Some(Value::Object(Some(result))))
    });

    r.register(path, "toFile", "()Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let file = alloc_concurrent_synthetic(ctx, "java/io/File", 1);
        // Pin across the create_string below — a moving young GC there would
        // relocate the fresh File (native stale-local family).
        let file_pin = ctx.pin_native_root(file);
        let s = ctx.create_string(&p);
        let file = ctx.read_native_pin(file_pin, file);
        ctx.set_field(file, 0, Value::Object(Some(s)));
        ctx.unpin_native_roots(file_pin);
        Ok(Some(Value::Object(Some(file))))
    });

    r.register(path, "toUri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        // jar-FS Path → `jar:file:/<jar>!/<entry>` URI (matches JDK zipfs), so
        // callers that round-trip back through URL/openStream can re-open it.
        if let Some((jar, entry)) = jarfs_decode(&p) {
            let jar_slash = jar.replace('\\', "/");
            let jar_abs = if jar_slash.starts_with('/') {
                jar_slash
            } else {
                format!("/{jar_slash}")
            };
            let entry = entry.trim_start_matches('/');
            let uri_str = format!("jar:file:{jar_abs}!/{entry}");
            let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 5);
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh URI (native stale-local family).
            let uri_pin = ctx.pin_native_root(uri);
            let s = ctx.create_string(&uri_str);
            let uri = ctx.read_native_pin(uri_pin, uri);
            ctx.set_field(uri, 0, Value::Object(Some(s)));
            ctx.set_field(uri, 4, Value::Object(Some(s)));
            ctx.unpin_native_roots(uri_pin);
            return Ok(Some(Value::Object(Some(uri))));
        }
        // jrt-FS Path → `jrt:/modules/<module>/<entry>` URI (matches the JDK
        // runtime-image scheme).
        if let Some((_jh, entry)) = jrtfs_decode(&p) {
            let entry = entry.trim_start_matches('/');
            let uri_str = format!("jrt:/{entry}");
            let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 5);
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh URI (native stale-local family).
            let uri_pin = ctx.pin_native_root(uri);
            let s = ctx.create_string(&uri_str);
            let uri = ctx.read_native_pin(uri_pin, uri);
            ctx.set_field(uri, 0, Value::Object(Some(s)));
            ctx.set_field(uri, 4, Value::Object(Some(s)));
            ctx.unpin_native_roots(uri_pin);
            return Ok(Some(Value::Object(Some(uri))));
        }
        let prefixed;
        let slash_p: &str = if p.starts_with('/') {
            &p
        } else {
            prefixed = format!("/{}", p);
            &prefixed
        };
        // Percent-encode the path component, matching `java.nio.file.Path.toUri()`
        // (and `File.toURI()`): a path char like `#`, ` `, `?` must be `%`-escaped
        // so it stays part of the path rather than being parsed as a URI fragment
        // or query. HotSpot renders `…/resource#test1.txt` as
        // `file:///…/resource%23test1.txt`; leaving the `#` literal made
        // `toUri().toURL()` drop everything after it (Spring's
        // PathMatchingResourcePatternResolver URL/URI-syntax assertions).
        // Per `UnixUriUtils.toUri`/`WindowsUriSupport.toUri`, a Path that
        // names an existing DIRECTORY renders with a trailing `/`; a file (or a
        // path that does not exist) does not. `java.io.File.toURI()` below
        // already applies the same rule. Until `Path` construction normalized
        // its stored string this was masked for paths the caller happened to
        // write with a trailing separator, and wrong for every other directory.
        let dir_slash;
        let slash_p: &str = if !slash_p.ends_with('/') && std::path::Path::new(&p).is_dir() {
            dir_slash = format!("{slash_p}/");
            &dir_slash
        } else {
            slash_p
        };
        let encoded = encode_file_uri_path(slash_p);
        let uri_str = format!("file://{}", encoded);
        let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 5);
        // Pin across the create_strings below — a moving young GC there would
        // relocate the fresh URI (native stale-local family).
        let uri_pin = ctx.pin_native_root(uri);
        let s = ctx.create_string(&uri_str);
        let uri = ctx.read_native_pin(uri_pin, uri);
        ctx.set_field(uri, 0, Value::Object(Some(s)));
        // field 4 = (decoded) path component, with the leading-slash form the JDK
        // exposes via `URI.getPath()` (e.g. `/C:/…/resource#test1.txt`).
        let path_s = ctx.create_string(slash_p);
        let uri = ctx.read_native_pin(uri_pin, uri);
        ctx.set_field(uri, 4, Value::Object(Some(path_s)));
        ctx.unpin_native_roots(uri_pin);
        Ok(Some(Value::Object(Some(uri))))
    });

    r.register(path, "getNameCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let count = p57_name_elements(&p).len();
        Ok(Some(Value::Int(count as i32)))
    });

    r.register(path, "getName", "(I)Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let p = p57_read_path(ctx, this);
        let parts: Vec<String> = p57_name_elements(&p);
        // FIX (finding 4): JDK throws IllegalArgumentException for out-of-range index.
        if idx < 0 || idx as usize >= parts.len() {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid index: {idx}"),
            }
            .into());
        }
        let name = parts[idx as usize].as_str();
        let result = p57_alloc_path(ctx, name);
        Ok(Some(Value::Object(Some(result))))
    });

    r.register(
        path,
        "startsWith",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, this);
            let o = p57_read_path(ctx, other);
            Ok(Some(Value::Int(if p.starts_with(&o) { 1 } else { 0 })))
        },
    );

    r.register(path, "startsWith", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let p = p57_read_path(ctx, this);
        let o = ctx.read_string(other).unwrap_or_default();
        Ok(Some(Value::Int(if p.starts_with(&o) { 1 } else { 0 })))
    });

    r.register(path, "endsWith", "(Ljava/nio/file/Path;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let p = p57_read_path(ctx, this);
        let o = p57_read_path(ctx, other);
        Ok(Some(Value::Int(if p.ends_with(&o) { 1 } else { 0 })))
    });

    r.register(path, "endsWith", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let p = p57_read_path(ctx, this);
        let o = ctx.read_string(other).unwrap_or_default();
        Ok(Some(Value::Int(if p.ends_with(&o) { 1 } else { 0 })))
    });

    r.register(path, "isAbsolute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        // keycloak-15: drive-relative / driveless-rooted paths are not absolute on
        // Windows (see `p57_win_is_absolute`); POSIX leading-`/` rule on Unix.
        let abs = if cfg!(windows) {
            p57_win_is_absolute(&p)
        } else {
            p.starts_with('/')
        };
        Ok(Some(Value::Int(if abs { 1 } else { 0 })))
    });

    r.register(path, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, this);
        let parts: Vec<String> = p57_name_elements(&p);
        use cratonvm_types::ArrayElementType;
        let arr = ctx.new_array(ArrayElementType::Reference, parts.len());
        // Pin across the Path/iterator allocs below — a moving young GC there
        // would relocate the fresh array (native stale-local family).
        let arr_pin = ctx.pin_native_root(arr);
        for (i, part) in parts.iter().enumerate() {
            let ep = p57_alloc_path(ctx, part);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(ep)));
        }
        let iter = alloc_concurrent_synthetic(ctx, "java/util/Iterator", 2);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_field(iter, 0, Value::Object(Some(arr)));
        ctx.set_field(iter, 1, Value::Int(0)); // cursor
        ctx.unpin_native_roots(arr_pin);
        Ok(Some(Value::Object(Some(iter))))
    });

    r.register(path, "subpath", "(II)Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let begin = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let end = match args[2] {
            Value::Int(i) => i,
            _ => 0,
        };
        let p = p57_read_path(ctx, this);
        let parts: Vec<String> = p57_name_elements(&p);
        let count = parts.len() as i32;
        // FIX (finding 4): JDK throws IllegalArgumentException for an out-of-range range.
        if begin < 0 || begin >= count || end <= begin || end > count {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid subpath range: begin={begin}, end={end}, count={count}"),
            }
            .into());
        }
        let sub: Vec<String> = parts[begin as usize..end as usize].to_vec();
        let result = p57_alloc_path(ctx, &sub.join("/"));
        Ok(Some(Value::Object(Some(result))))
    });

    r.register(path, "compareTo", "(Ljava/nio/file/Path;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let p = p57_read_path(ctx, this);
        let o = p57_read_path(ctx, other);
        Ok(Some(Value::Int(p.cmp(&o) as i32)))
    });

    // --- Paths utility class ---
    let paths = "java/nio/file/Paths";
    r.register(
        paths,
        "get",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            let first = obj_arg(args, 0)?;
            let mut p = ctx.read_string(first).unwrap_or_default();
            // Varargs more[] - field 0 of the array
            if let Value::Object(Some(arr)) = &args[1] {
                let len = ctx.array_length(*arr);
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                        let part = ctx.read_string(s).unwrap_or_default();
                        if !p.ends_with('/') && !part.starts_with('/') {
                            p.push('/');
                        }
                        p.push_str(&part);
                    }
                }
            }
            p57_alloc_path_checked(ctx, &p)
        },
    );

    // --- FileSystem additional methods ---
    let fsys = "java/nio/file/FileSystem";
    // `newWatchService` is NOT registered here either — see the matching note
    // on the `fs_class` block above. This copy allocated a ZERO-field
    // WatchService, which is the object the heap guard reported as
    // "out-of-bounds field write dropped ... class_name=java/nio/file/
    // WatchService real_field_count=Some(0)".

    r.register(
        fsys,
        "getPathMatcher",
        "(Ljava/lang/String;)Ljava/nio/file/PathMatcher;",
        |ctx, _args| {
            let pm = alloc_concurrent_synthetic(ctx, "java/nio/file/PathMatcher", 0);
            Ok(Some(Value::Object(Some(pm))))
        },
    );

    // The (class, method, descriptor) triples a JDK 25 image declares
    // ACC_NATIVE, measured against linux-x64 AND windows-x64 25.0.4+7 on
    // 2026-08-05. `java.io.UnixFileSystem` exists only on the Linux image and
    // `java.io.WinNTFileSystem` only on the Windows one, so each is judged
    // against its own platform and the union is what this table holds.
    //
    // Everything registered below that is NOT in here is one of two things:
    // the un-suffixed spelling, which is the Java wrapper that calls the JNI
    // entry point (a contract §1.4 shadow, not a §1.5 bridge), or a
    // descriptor/name the other platform uses. Both keep the ambient category
    // so the census goes on reporting them as unadjudicated.
    const FS_IMAGE_NATIVE: &[(&str, &str, &str)] = &[
    ("java/io/UnixFileSystem", "canonicalize0", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/io/UnixFileSystem", "checkAccess0", "(Ljava/io/File;I)Z"),
    ("java/io/UnixFileSystem", "createDirectory0", "(Ljava/io/File;)Z"),
    ("java/io/UnixFileSystem", "createFileExclusively0", "(Ljava/lang/String;)Z"),
    ("java/io/UnixFileSystem", "delete0", "(Ljava/io/File;)Z"),
    ("java/io/UnixFileSystem", "getBooleanAttributes0", "(Ljava/io/File;)I"),
    ("java/io/UnixFileSystem", "getLastModifiedTime0", "(Ljava/io/File;)J"),
    ("java/io/UnixFileSystem", "getLength0", "(Ljava/io/File;)J"),
    ("java/io/UnixFileSystem", "getNameMax0", "(Ljava/lang/String;)J"),
    ("java/io/UnixFileSystem", "getSpace0", "(Ljava/io/File;I)J"),
    ("java/io/UnixFileSystem", "initIDs", "()V"),
    ("java/io/UnixFileSystem", "list0", "(Ljava/io/File;)[Ljava/lang/String;"),
    ("java/io/UnixFileSystem", "rename0", "(Ljava/io/File;Ljava/io/File;)Z"),
    ("java/io/UnixFileSystem", "setLastModifiedTime0", "(Ljava/io/File;J)Z"),
    ("java/io/UnixFileSystem", "setPermission0", "(Ljava/io/File;IZZ)Z"),
    ("java/io/UnixFileSystem", "setReadOnly0", "(Ljava/io/File;)Z"),
    ("java/io/WinNTFileSystem", "canonicalize0", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/io/WinNTFileSystem", "checkAccess0", "(Ljava/io/File;I)Z"),
    ("java/io/WinNTFileSystem", "createDirectory0", "(Ljava/io/File;)Z"),
    ("java/io/WinNTFileSystem", "createFileExclusively0", "(Ljava/lang/String;)Z"),
    ("java/io/WinNTFileSystem", "delete0", "(Ljava/io/File;Z)Z"),
    ("java/io/WinNTFileSystem", "getBooleanAttributes0", "(Ljava/io/File;)I"),
    ("java/io/WinNTFileSystem", "getDriveDirectory", "(I)Ljava/lang/String;"),
    ("java/io/WinNTFileSystem", "getFinalPath0", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/io/WinNTFileSystem", "getLastModifiedTime0", "(Ljava/io/File;)J"),
    ("java/io/WinNTFileSystem", "getLength0", "(Ljava/io/File;)J"),
    ("java/io/WinNTFileSystem", "getNameMax0", "(Ljava/lang/String;)I"),
    ("java/io/WinNTFileSystem", "getSpace0", "(Ljava/io/File;I)J"),
    ("java/io/WinNTFileSystem", "initIDs", "()V"),
    ("java/io/WinNTFileSystem", "list0", "(Ljava/io/File;)[Ljava/lang/String;"),
    ("java/io/WinNTFileSystem", "listRoots0", "()I"),
    ("java/io/WinNTFileSystem", "rename0", "(Ljava/io/File;Ljava/io/File;)Z"),
    ("java/io/WinNTFileSystem", "setLastModifiedTime0", "(Ljava/io/File;J)Z"),
    ("java/io/WinNTFileSystem", "setPermission0", "(Ljava/io/File;IZZ)Z"),
    ("java/io/WinNTFileSystem", "setReadOnly0", "(Ljava/io/File;)Z"),
    ];
    /// Register a `FileSystem` native, stating `Bridge` only when the image
    /// backs this exact triple. See `FS_IMAGE_NATIVE`.
    fn fs_reg(
        r: &mut NativeMethodRegistry,
        cls: &str,
        name: &str,
        desc: &str,
        cb: cratonvm_native_api::NativeCallback,
    ) {
        if FS_IMAGE_NATIVE.contains(&(cls, name, desc)) {
            r.register_with_kind(cls, name, desc, cb, cratonvm_native_api::NativeKind::Bridge);
        } else {
            r.register(cls, name, desc, cb);
        }
    }

    // --- WinNTFileSystem / UnixFileSystem native methods ---
    //
    // These are JNI natives in the real JDK. We implement them using std::fs.
    //
    // EVERY operation is registered under BOTH spellings — the bare pre-JDK-22
    // name and the `0`-suffixed JDK-22+ name — because the two JDK generations
    // put the native in different places and only one of them is ever called
    // on a given JDK. See the `fs_boolean_attributes0` banner in this file for
    // why registering only the bare name silently registers a method nothing
    // calls on a modern JDK.
    for fs_cls in &["java/io/WinNTFileSystem", "java/io/UnixFileSystem"] {
        r.register_with_kind(
            fs_cls,
            "canonicalize0",
            "(Ljava/lang/String;)Ljava/lang/String;",
            |ctx, args| {
                let path_ref = obj_arg(args, 1)?;
                let p = ctx.read_string(path_ref).unwrap_or_default();
                // Normalize even for non-existent paths: make absolute and
                // collapse `.`/`..` so containment checks behave like the
                // real JDK. Strips the `\\?\` extended-length prefix too.
                let canonical = file_canonicalize_path(&p);
                let s = ctx.create_string(&canonical);
                Ok(Some(Value::Object(Some(s))))
            },
            cratonvm_native_api::NativeKind::Bridge,
        );

        // getBooleanAttributes returns a bitmask:
        // BA_EXISTS=0x01, BA_REGULAR=0x02, BA_DIRECTORY=0x04, BA_HIDDEN=0x08
        //
        // `getBooleanAttributes0` is the raw JNI native (JDK 22+) and
        // `getBooleanAttributes` the pre-22 spelling / public wrapper; they
        // differ only in BA_HIDDEN, which is why they get different bodies.
        r.register_with_kind(
            fs_cls,
            "getBooleanAttributes0",
            "(Ljava/io/File;)I",
            |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                Ok(Some(Value::Int(fs_boolean_attributes0(&path))))
            },
            cratonvm_native_api::NativeKind::Bridge,
        );
        fs_reg(
            r,
            fs_cls,
            "getBooleanAttributes",
            "(Ljava/io/File;)I",
            |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                Ok(Some(Value::Int(fs_boolean_attributes(&path))))
            },
        );

        for name in ["getLastModifiedTime", "getLastModifiedTime0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;)J", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let millis = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64
                    })
                    .unwrap_or(0);
                Ok(Some(Value::Long(millis)))
            });
        }

        for name in ["getLength", "getLength0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;)J", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let len = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);
                Ok(Some(Value::Long(len)))
            });
        }

        for name in ["list", "list0"] {
            fs_reg(
                r,
                fs_cls,
                name,
                "(Ljava/io/File;)[Ljava/lang/String;",
                |ctx, args| {
                    let file_ref = obj_arg(args, 1)?;
                    let path = file_read_path(ctx, file_ref);
                    // `null`, not an empty array, when the path is not a
                    // readable directory — `File.list()` documents the
                    // difference and callers branch on it.
                    let entries = match fs_list_dir(&path) {
                        Some(e) => e,
                        None => return Ok(Some(Value::Object(None))),
                    };
                    // A `String[]`, not the untyped `Object[]` that
                    // `new_array(ArrayElementType::Reference, ..)` produces:
                    // the descriptor is `[Ljava/lang/String;` and
                    // `File.normalizedList()` assigns the result straight to a
                    // `String[]` local, which is a checkcast.
                    let string_class = string_class_id(ctx);
                    let arr = ctx.new_ref_array(string_class, entries.len());
                    // Pin across the `create_string` calls below — a moving
                    // young GC there relocates the fresh array (the native
                    // stale-local family), same as `java/io/File.list()`.
                    let arr_pin = ctx.pin_native_root(arr);
                    for (i, name) in entries.iter().enumerate() {
                        let s = ctx.create_string(name);
                        let arr = ctx.read_native_pin(arr_pin, arr);
                        ctx.set_array_element(arr, i, Value::Object(Some(s)));
                    }
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.unpin_native_roots(arr_pin);
                    Ok(Some(Value::Object(Some(arr))))
                },
            );
        }

        for name in ["createDirectory", "createDirectory0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;)Z", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let ok = std::fs::create_dir(&path).is_ok();
                Ok(Some(Value::Int(if ok { 1 } else { 0 })))
            });
        }

        r.register_with_kind(
            fs_cls,
            "rename0",
            "(Ljava/io/File;Ljava/io/File;)Z",
            |ctx, args| {
                let from_ref = obj_arg(args, 1)?;
                let to_ref = obj_arg(args, 2)?;
                let from = file_read_path(ctx, from_ref);
                let to = file_read_path(ctx, to_ref);
                let ok = std::fs::rename(&from, &to).is_ok();
                Ok(Some(Value::Int(if ok { 1 } else { 0 })))
            },
            cratonvm_native_api::NativeKind::Bridge,
        );

        fs_reg(r, fs_cls, "delete0", "(Ljava/io/File;)Z", |ctx, args| {
            let file_ref = obj_arg(args, 1)?;
            let path = file_read_path(ctx, file_ref);
            let ok = std::fs::remove_file(&path)
                .or_else(|_| std::fs::remove_dir(&path))
                .is_ok();
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        });
        // `WinNTFileSystem.delete0` takes a second `allowDeleteReadOnlyFiles`
        // argument (`java.io.File.allowDeleteReadOnlyFiles`, default false)
        // that `UnixFileSystem.delete0` does not have — a different method,
        // not a different spelling, so it needs its own registration or
        // `File.delete()` is an `UnsatisfiedLinkError` on Windows.
        fs_reg(r, fs_cls, "delete0", "(Ljava/io/File;Z)Z", |ctx, args| {
            let file_ref = obj_arg(args, 1)?;
            let allow_read_only = matches!(args.get(2), Some(v) if v.as_int().unwrap_or(0) != 0);
            let path = file_read_path(ctx, file_ref);
            if allow_read_only {
                if let Ok(md) = std::fs::metadata(&path) {
                    let mut perms = md.permissions();
                    if perms.readonly() {
                        // `set_readonly(false)` is right on Windows, where it
                        // clears FILE_ATTRIBUTE_READONLY and that IS the whole
                        // operation `allowDeleteReadOnlyFiles` asks for. On Unix
                        // the same call writes mode 0o666 — it grants write to
                        // group and other as well, which is a permission
                        // widening nobody asked for (clippy::
                        // permissions_set_readonly_false). Restore the owner
                        // write bit only; that is the Unix reading of "make it
                        // writable again", and deletion there depends on the
                        // DIRECTORY's mode anyway.
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let mode = perms.mode();
                            perms.set_mode(mode | 0o200);
                        }
                        #[cfg(not(unix))]
                        perms.set_readonly(false);
                        let _ = std::fs::set_permissions(&path, perms);
                    }
                }
            }
            let ok = std::fs::remove_file(&path)
                .or_else(|_| std::fs::remove_dir(&path))
                .is_ok();
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        });

        for name in ["setLastModifiedTime", "setLastModifiedTime0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;J)Z", |ctx, args| {
                // Real-JDK `File.setLastModified(long)` bytecode delegates here.
                // This used to be a stub that claimed success without touching
                // the file, so any caller that reached the bytecode path (rather
                // than the direct `java/io/File.setLastModified` native below)
                // got `true` and an unchanged timestamp. Share the same helper
                // so both entry points behave identically for files AND
                // directories.
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let millis = match args.get(2) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                Ok(Some(Value::Int(if set_file_mtime_millis(&path, millis) {
                    1
                } else {
                    0
                })))
            });
        }

        // Was a hardcoded `true`: `File.setReadOnly()`'s real bytecode
        // delegates here, so a caller was told the file had been made
        // read-only while its permissions were untouched — and a follow-up
        // `canWrite()` (which IS a real permission query) then contradicted
        // it. Same body as the direct `java/io/File.setReadOnly()` native.
        for name in ["setReadOnly", "setReadOnly0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;)Z", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let ok = std::fs::metadata(&path)
                    .and_then(|meta| {
                        let mut perms = meta.permissions();
                        perms.set_readonly(true);
                        std::fs::set_permissions(&path, perms)
                    })
                    .is_ok();
                Ok(Some(Value::Int(if ok { 1 } else { 0 })))
            });
        }

        // `File.setWritable`/`setReadable`/`setExecutable` bytecode lands here.
        // Never registered before, so all three were an `UnsatisfiedLinkError`
        // for any caller that reached the real `FileSystem` bytecode.
        for name in ["setPermission", "setPermission0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;IZZ)Z", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let access = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let enable = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) != 0;
                let owner_only = args.get(4).and_then(|v| v.as_int()).unwrap_or(0) != 0;
                let ok = fs_set_permission(&path, access, enable, owner_only);
                Ok(Some(Value::Int(if ok { 1 } else { 0 })))
            });
        }

        // `File.createNewFile()` / `File.createTempFile(...)` bytecode. Same
        // story as `setPermission`: never registered under either spelling.
        for name in ["createFileExclusively", "createFileExclusively0"] {
            fs_reg(r, fs_cls, name, "(Ljava/lang/String;)Z", |ctx, args| {
                let path_ref = obj_arg(args, 1)?;
                let path = ctx.read_string(path_ref).unwrap_or_default();
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(_) => Ok(Some(Value::Int(1))),
                    // The JDK returns false only for "already exists"; every
                    // other errno is an IOException the caller must see.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        Ok(Some(Value::Int(0)))
                    }
                    Err(e) => Err(RuntimeError::IOException {
                        message: format!("{path}: {e}"),
                    }
                    .into()),
                }
            });
        }

        // real-JDK `File.getTotalSpace()`/`getFreeSpace()`/`getUsableSpace()`
        // delegate to `FileSystem.getSpace(File, int)` (SPACE_TOTAL=0,
        // SPACE_FREE=1, SPACE_USABLE=2) rather than being native themselves —
        // this path is normally shadowed by the direct natives registered on
        // `java/io/File` itself (see `file_disk_space_bytes` below), but once
        // any test in the process instruments `java.io.File` via Mockito's
        // inline mock maker (`@Mock private File f`), the real (redefined)
        // `File` bytecode runs and reaches this native instead — must return
        // the same real values, not a hardcoded stub, or a mixed
        // mocked/real-File test class (e.g. `DiskSpaceHealthIndicatorTests`)
        // gets a correct answer for the mocked instances but a fake one for
        // real `File`s in the same JVM process.
        for name in ["getSpace", "getSpace0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;I)J", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let space_type = match args.get(2) {
                    Some(Value::Int(v)) => *v,
                    _ => 2,
                };
                let path = file_read_path(ctx, file_ref);
                let value = file_disk_space_bytes(&path).map_or(0, |(total, free, usable)| {
                    match space_type {
                        0 => total,
                        1 => free,
                        _ => usable,
                    }
                });
                Ok(Some(Value::Long(value as i64)))
            });
        }

        // Was "does the path exist?" for every mode, so `canWrite()` said
        // `true` for a file `setReadOnly()` had just locked down.
        for name in ["checkAccess", "checkAccess0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;I)Z", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let access = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                Ok(Some(Value::Int(if fs_check_access(&path, access) {
                    1
                } else {
                    0
                })))
            });
        }

        // Was a flat 255. Unix can answer this exactly the way the JDK's own
        // `UnixFileSystem.getNameMax0` does — `pathconf(_PC_NAME_MAX)` — and
        // that is not always 255 (an eCryptfs mount reports 143, and callers
        // like Lucene/H2 use the value to decide how long a generated file name
        // may be, so an over-long name fails at create time instead).
        //
        // The return type is NOT the same on both platforms: `long` on
        // `UnixFileSystem`, `int` on `WinNTFileSystem`. Registering only the
        // `I` form left the Unix native unresolved, since a native is looked
        // up by descriptor as well as name.
        fs_reg(
            r,
            fs_cls,
            "getNameMax0",
            "(Ljava/lang/String;)I",
            |ctx, args| {
                let path = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                Ok(Some(Value::Int(file_system_name_max(&path))))
            },
        );
        fs_reg(
            r,
            fs_cls,
            "getNameMax0",
            "(Ljava/lang/String;)J",
            |ctx, args| {
                let path = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                Ok(Some(Value::Long(file_system_name_max(&path) as i64)))
            },
        );

        // `WinNTFileSystem.canonicalize(String)` is
        // `getFinalPath(canonicalize0(s))`, so this native's result IS what a
        // caller reaching real `FileSystem` bytecode gets back. It used to
        // return `std::fs::canonicalize`'s output verbatim, which on Windows
        // carries the `\\?\` extended-length prefix that the real
        // `GetFinalPathNameByHandleW`-based native strips: `canonicalize` came
        // back as `\\?\C:\...\x.txt` where HotSpot returns `C:\...\x.txt`, so
        // every `startsWith(canonicalBase)` containment check against a
        // non-prefixed base failed. `strip_unc` is the same helper
        // `file_canonicalize_path_uncached` already applies for this reason.
        fs_reg(
            r,
            fs_cls,
            "getFinalPath0",
            "(Ljava/lang/String;)Ljava/lang/String;",
            |ctx, args| {
                let path_ref = obj_arg(args, 1)?;
                let p = ctx.read_string(path_ref).unwrap_or_default();
                let final_path = std::fs::canonicalize(&p)
                    .map(|c| strip_unc(&c.to_string_lossy()))
                    .unwrap_or(p);
                let s = ctx.create_string(&final_path);
                Ok(Some(Value::Object(Some(s))))
            },
        );

        // --- WinNTFileSystem-only natives ---
        // Registered on both class names for symmetry with the loop; only the
        // Windows class ever declares them, and a native is only reachable
        // through a declared method.

        // `File.listRoots()` -> a bitmask with bit 0 = `A:`.
        fs_reg(r, fs_cls, "listRoots0", "()I", |_ctx, _args| {
            Ok(Some(Value::Int(fs_list_roots_bitmask())))
        });

        // The per-drive working directory, minus its `X:` prefix — what
        // `_wgetdcwd` gives the JDK. Reached only for drive-relative paths
        // (`C:foo`), which `WinNTFileSystem.resolve` has to expand.
        fs_reg(
            r,
            fs_cls,
            "getDriveDirectory",
            "(I)Ljava/lang/String;",
            |ctx, args| {
                let drive = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                if !(1..=26).contains(&drive) {
                    return Ok(Some(Value::Object(None)));
                }
                let letter = (b'A' + (drive - 1) as u8) as char;
                // Windows keeps each drive's working directory in a hidden
                // `=X:` environment variable; that IS what `_wgetdcwd` reads.
                let dir = cratonvm_types::flags::runtime_var(format!("={letter}:"))
                    .ok()
                    .or_else(|| {
                        let cwd = std::env::current_dir().ok()?;
                        let cwd = cwd.to_string_lossy().to_string();
                        cwd.starts_with(&format!("{letter}:")).then_some(cwd)
                    })
                    .unwrap_or_else(|| format!("{letter}:\\"));
                // Strip the `X:` prefix the JDK's native also strips.
                let stripped = dir.get(2..).unwrap_or("\\").to_string();
                let s = ctx.create_string(&stripped);
                Ok(Some(Value::Object(Some(s))))
            },
        );

        // The real native caches jfieldIDs; there is nothing for us to cache,
        // but the class's `<clinit>` calls it and an unregistered native there
        // fails class initialisation outright.
        r.register_with_kind(fs_cls, "initIDs", "()V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge);
    }

    // Round 24 — Files.write(Path, Iterable<? extends CharSequence>, OpenOption...)
    // and the (..., Charset, ...) overload. Used by JBoss
    // ProcessEnvironment.obtainProcessUUID to write standalone/data/process.uuid.
    // Also Files.write(Path, byte[], OpenOption...) — needed in real-JDK mode
    // where register_p71_files_bridge is not invoked. Without these, the JDK
    // bytecode falls through to FileSystemProvider.newOutputStream which we
    // do not implement and throws "Не удается найти указанный файл" (errno 2).
    let files_cls = "java/nio/file/Files";
    r.register(
        files_cls,
        "write",
        "(Ljava/nio/file/Path;[B[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        files_write_bytes_impl,
    );
    fn write_iterable_impl(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        let path_obj = obj_arg(args, 0)?;
        let iterable = obj_arg(args, 1)?;
        let options = args.get(2).copied();
        let opts_obj = match options {
            Some(Value::Object(Some(o))) => Some(o),
            _ => None,
        };
        // Everything below re-enters Java repeatedly (iterator/hasNext/next/
        // toString), so the Path, the OpenOption[] and the Iterator all have to
        // be pinned — `path_pin` FIRST, since unpinning is by batch base.
        let path_pin = ctx.pin_native_root(path_obj);
        let opts_pin = opts_obj.map(|o| ctx.pin_native_root(o));
        let it_val = ctx
            .invoke_virtual(iterable, "iterator", "()Ljava/util/Iterator;", &[])
            .ok()
            .flatten();
        let it = match it_val {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(path_pin);
                return Err(RuntimeError::IOException {
                    message: "Files.write(Iterable): null iterator".to_string(),
                }
                .into());
            }
        };
        let it_pin = ctx.pin_native_root(it);
        let mut out = String::new();
        loop {
            let it = ctx.read_native_pin(it_pin, it);
            let has = ctx.invoke_virtual(it, "hasNext", "()Z", &[]).ok().flatten();
            match has {
                Some(Value::Int(1)) => {}
                _ => break,
            }
            let it = ctx.read_native_pin(it_pin, it);
            let nxt = ctx
                .invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])
                .ok()
                .flatten();
            let elem = match nxt {
                Some(Value::Object(Some(o))) => o,
                _ => break,
            };
            // CharSequence: try read_string first (Strings), fall back to toString().
            let s = ctx.read_string(elem).unwrap_or_else(|| {
                let s_val = ctx
                    .invoke_virtual(elem, "toString", "()Ljava/lang/String;", &[])
                    .ok()
                    .flatten();
                match s_val {
                    Some(Value::Object(Some(so))) => ctx.read_string(so).unwrap_or_default(),
                    _ => String::new(),
                }
            });
            out.push_str(&s);
            out.push('\n');
        }
        // The iteration above re-entered Java (`hasNext`/`next`/`toString`), so
        // the pinned `Path` is the only reference still safe to return.
        let path_obj = ctx.read_native_pin(path_pin, path_obj);
        let options = match (opts_obj, opts_pin) {
            (Some(o), Some(h)) => Some(Value::Object(Some(ctx.read_native_pin(h, o)))),
            _ => None,
        };
        ctx.unpin_native_roots(path_pin);
        p57_files_write_bytes(ctx, path_obj, out.as_bytes(), options)
    }
    r.register(
        files_cls,
        "write",
        "(Ljava/nio/file/Path;Ljava/lang/Iterable;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        write_iterable_impl,
    );
    r.register(
        files_cls,
        "write",
        "(Ljava/nio/file/Path;Ljava/lang/Iterable;Ljava/nio/charset/Charset;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        |ctx, args| {
            // Charset arg at slot 2 ignored — we always emit UTF-8, matching
            // the platform default we report from `Charset.defaultCharset`.
            // Forward the (path, iterable, options) shape by reordering.
            let trimmed: Vec<Value> = vec![args[0], args[1], args.get(3).copied().unwrap_or(Value::Object(None))];
            write_iterable_impl(ctx, &trimmed)
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) fn p57_read_path(ctx: &mut dyn NativeContext, path_obj: ObjectRef) -> String {
    // Fast path: CratonVM's own synthetic Path layout stores the path String
    // directly at field 0 (P57_PATH_FIELD).
    //
    // Real-JDK Path implementations we don't control the layout of -- e.g. a
    // decorator/wrapper Path like Quarkus's `io.quarkus.fs.util.sysfs.
    // PathWrapper` (extends `DelegatingPath`, whose field 0 is the wrapped
    // delegate Path OBJECT, not a String) -- do NOT share this layout.
    // `io.quarkus.fs.util.FileSystemHelper.ignoreFileWriteability` wraps
    // every Path in exactly this decorator before `ZipUtils.unzip()` mounts
    // it, so `FileSystemProvider.newFileSystem(Path,Map)`'s jar-path
    // extraction (below) was blindly field-0-reading a PathWrapper and
    // silently getting back "" (read_string correctly refuses to
    // mis-interpret the delegate-Path object as a String, but the old code
    // treated that failure as "no path" rather than "wrong layout").  An
    // empty jar path made `p57_alloc_jar_filesystem` skip mounting the
    // archive at all, so the resulting FileSystem's `getRootDirectories()`
    // fell back to the REAL host filesystem root -- `Files.walkFileTree`
    // over "/" then tried to copy the ENTIRE host filesystem into the
    // extraction target (observed: ~6GB and climbing, "No space left on
    // device", then an `IOException: the source path is neither a regular
    // file nor a symlink to a regular file` once it reached a device/socket
    // special file under the real root).
    //
    // Fall back to a real virtual dispatch to `toString()` -- which every
    // concrete Path (including delegating wrappers, via their real bytecode
    // delegating implementation) implements correctly -- whenever the fast
    // path doesn't yield a String.
    if let Value::Object(Some(s)) = ctx.get_field(path_obj, P57_PATH_FIELD) {
        if let Some(raw) = ctx.read_string(s) {
            return p57_to_os_path(&raw);
        }
    }
    // STACK-OVERFLOW GUARD (found 2026-07-19 investigating a real crash: JIT
    // method-stats work on an unrelated ES/Lucene test hit
    // EXCEPTION_STACK_OVERFLOW at gen_heap::get_field, root-caused via the
    // dispatch_trace ring to `TestRuleTemporaryFilesCleanup.initializeJavaTempDir`
    // -> native Path.toString() repeating 256/256 times with zero variation).
    //
    // This fallback's `invoke_virtual(path_obj, "toString", ...)` was written
    // assuming dispatch lands somewhere OTHER than back here — either the
    // (separate) dead-dispatch-to-Object.toString() bug fixed the same day in
    // `fixed-suite-bugs/springboot/path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`,
    // or a genuine delegating wrapper's own real bytecode `toString()`. A
    // THIRD same-day fix
    // (`fixed-suite-bugs/springboot/path-tostring-indy-stringconcat-dead-dispatch-FIXED.md`,
    // `vm_exec.rs`'s `invoke_on_class_shared_inner`) made dispatch correctly
    // receiver-aware: ANY Path-subtype receiver's `toString()` now routes
    // straight back to this exact native (`p57_path_display_string` ->
    // `p57_read_path`). For a `path_obj` whose field-0 fast-path read keeps
    // failing (nothing about the object changes between calls), that
    // redirect recurses into this same function forever — a real, silent
    // EXCEPTION_STACK_OVERFLOW, not a Java StackOverflowError (native
    // recursion via `ctx.invoke_virtual` is invisible to every one of the
    // interpreter's counted recursion guards; see
    // `fixed-suite-bugs/elasticsearch-suite/ES-CRASH-20260719-lucene-jit-getfield-stack-overflow-FIXED.md`).
    //
    // A thread-local re-entrancy flag breaks the cycle: the first call takes
    // the real dispatch as before (the common, legitimate delegating-wrapper
    // case terminates immediately since IT dispatches into different, real
    // bytecode); a NESTED re-entry into this exact fallback — which can only
    // happen via the recursive-redirect case above — returns the same benign
    // empty-string fallback the final `_ =>` arm already uses for other
    // failures, instead of recursing again.
    thread_local! {
        static IN_TOSTRING_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if IN_TOSTRING_FALLBACK.with(|f| f.get()) {
        return String::new();
    }
    IN_TOSTRING_FALLBACK.with(|f| f.set(true));
    let result = match ctx.invoke_virtual(path_obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => {
            let raw = ctx.read_string(s).unwrap_or_default();
            p57_to_os_path(&raw)
        }
        _ => String::new(),
    };
    IN_TOSTRING_FALLBACK.with(|f| f.set(false));
    result
}

/// Convert a Java-style path to an OS-native path.
/// Strips leading `/` from Windows drive paths like `/C:/foo` → `C:/foo`.
pub(crate) fn p57_to_os_path(p: &str) -> String {
    // Handle `/C:/...` — common from URI paths on Windows
    if cfg!(windows) && p.len() >= 3 && p.starts_with('/') && p.as_bytes().get(2) == Some(&b':') {
        return p[1..].to_string();
    }
    p.to_string()
}

pub(crate) fn p57_absolute_path_string(path: &str) -> String {
    // A mounted jar/jrt entry is absolute within its own filesystem. Several
    // Path.toAbsolutePath registrations share this helper; letting any one of
    // them anchor the opaque sentinel to the host CWD both leaks the sentinel
    // through Path.toString() and changes the entry identity. Jetty's
    // PathResource.getName() exercises exactly that sequence after listing a
    // `jar:` URI.
    if vfs_decode(path).is_some() {
        return path.to_string();
    }
    #[cfg(windows)]
    {
        return p57_windows_absolute_path_string(path);
    }
    #[cfg(not(windows))]
    {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            path.to_string()
        } else {
            std::env::current_dir()
                .unwrap_or_default()
                .join(p)
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// Match java.io.File's lexical normalization for ordinary absolute paths.
/// Preserve filesystem roots (and Windows drive roots) while removing an
/// otherwise-significant trailing separator.
pub(crate) fn p57_trim_file_trailing_separator(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return if path.starts_with('\\') {
            "\\".to_string()
        } else {
            "/".to_string()
        };
    }
    if trimmed.len() == 2
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
        && trimmed.as_bytes()[1] == b':'
    {
        return format!("{trimmed}/");
    }
    trimmed.to_string()
}

/// Match `WindowsPath` construction: an ordinary trailing separator is not a
/// name element, while every filesystem root keeps its separator. This is
/// separate from the `java.io.File` helper because virtual filesystem paths
/// carry opaque encoded roots that must never be rewritten.
#[cfg(windows)]
pub(crate) fn p57_trim_path_trailing_separator(path: &str) -> String {
    if jarfs_decode(path).is_some() || vfs_decode(path).is_some() || !path.ends_with(['/', '\\']) {
        return path.to_string();
    }
    let (root, names) = p57_parse_win_root(path);
    if root.is_some() && names.is_empty() {
        return path.to_string();
    }
    path.trim_end_matches(['/', '\\']).to_string()
}

/// Match `UnixPath` construction: `sun.nio.fs.UnixPath`'s constructor stores the
/// result of `normalizeAndCheck`, which collapses runs of `/` to a single
/// separator and drops a redundant trailing `/` (the root `/` keeps its own).
/// That is the same normalization `java.io.File` gets from
/// `UnixFileSystem.normalize` (see [`file_normalise_path`]) — on this platform
/// the two APIs share one rule, so the `Path` allocator delegates to it rather
/// than re-deriving it.
///
/// Without this the stored string kept whatever separators the caller wrote.
/// `Path.toString()` hid that (it renders through `file_normalise_path`), but
/// every consumer of the raw string saw it: `equals`/`hashCode`/`compareTo`/
/// `endsWith` disagreed with HotSpot, and the file-IO bridge received a
/// directory-shaped path — `Files.writeString(root.resolve("one/two/three/"), ...)`
/// failed with `EISDIR` ("Is a directory", errno 21) instead of creating the
/// file (Windows reported the same defect as `ERROR_DIRECTORY`/267). See
/// `fixed-suite-bugs/springboot/resourcestests-trailing-slash-path-normalization-FIXED-20260804.md`.
///
/// Virtual (jar/jrt) filesystem paths are excluded, exactly as in the Windows
/// twin: their sentinel-encoded string carries an entry whose trailing `/` is
/// part of the virtual-entry representation, not a redundant separator.
#[cfg(not(windows))]
pub(crate) fn p57_trim_path_trailing_separator(path: &str) -> String {
    if vfs_decode(path).is_some() {
        return path.to_string();
    }
    file_normalise_path(path)
}

#[cfg(windows)]
pub(crate) fn p57_windows_absolute_path_string(path: &str) -> String {
    let s = path.replace('\\', "/");
    let b = s.as_bytes();
    let is_sep = |c: u8| c == b'/' || c == b'\\';
    let has_drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let drive_absolute = has_drive && b.len() >= 3 && is_sep(b[2]);
    let unc_absolute = b.len() >= 2 && is_sep(b[0]) && is_sep(b[1]);
    if drive_absolute || unc_absolute {
        return s;
    }

    let mut cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .replace('\\', "/");
    if let Some(stripped) = cwd.strip_prefix("//?/") {
        cwd = stripped.to_string();
    }
    while cwd.len() > 3 && cwd.ends_with('/') {
        cwd.pop();
    }

    let cwd_drive = cwd
        .as_bytes()
        .get(0..2)
        .filter(|d| d[0].is_ascii_alphabetic() && d[1] == b':')
        .and_then(|_| cwd.get(0..2))
        .unwrap_or("");

    if b.first().is_some_and(|c| is_sep(*c)) {
        let rest = s.trim_start_matches(|c| c == '/' || c == '\\');
        if cwd_drive.is_empty() {
            return format!("/{rest}");
        }
        return format!("{cwd_drive}/{rest}");
    }

    if has_drive {
        let drive = &s[..2];
        let rest = s[2..].trim_start_matches(|c| c == '/' || c == '\\');
        if cwd.get(0..2).is_some_and(|d| d.eq_ignore_ascii_case(drive)) {
            if rest.is_empty() {
                return cwd;
            }
            return format!("{cwd}/{rest}");
        }
        if rest.is_empty() {
            return format!("{drive}/");
        }
        return format!("{drive}/{rest}");
    }

    if s.is_empty() {
        cwd
    } else {
        format!("{cwd}/{s}")
    }
}

/// keycloak-15: explicit Windows (sun.nio.fs.WindowsPath) root/name parsing for
/// the p57 path natives. The stored string is '/'-canonical (see `p57_alloc_path`),
/// and `std::path::Component` does not classify the drive/UNC prefix in this build,
/// so getRoot/getNameCount/getName must parse the prefix themselves. Accepts both
/// '/' and '\\' separators. Returns `(root, names-after-root)`: root is the JDK
/// root string (`C:\`, `\\server\share\`, `C:`, `\`) or None for a relative path.
pub(crate) fn p57_parse_win_root(s: &str) -> (Option<String>, Vec<String>) {
    let is_sep = |c: u8| c == b'\\' || c == b'/';
    let split_names = |rest: &str| -> Vec<String> {
        rest.split(|c| c == '\\' || c == '/')
            .filter(|seg| !seg.is_empty())
            .map(|seg| seg.to_string())
            .collect()
    };
    if let Some((_tag, _container, entry)) = vfs_decode(s) {
        let entry = entry.trim_start_matches('/');
        return (Some("/".to_string()), split_names(entry));
    }

    let (work, verbatim) = {
        let b = s.as_bytes();
        if b.len() >= 4 && is_sep(b[0]) && is_sep(b[1]) && b[2] == b'?' && is_sep(b[3]) {
            (&s[4..], true)
        } else {
            (s, false)
        }
    };
    let wb = work.as_bytes();
    if verbatim
        && wb.len() >= 4
        && work
            .get(..3)
            .map_or(false, |p| p.eq_ignore_ascii_case("UNC"))
        && is_sep(wb[3])
    {
        let after = &work[4..];
        let mut it = after.splitn(3, |c| c == '\\' || c == '/');
        let server = it.next().unwrap_or("");
        let share = it.next().unwrap_or("");
        let remainder = it.next().unwrap_or("");
        return (
            Some(format!("\\\\{}\\{}\\", server, share)),
            split_names(remainder),
        );
    }
    if wb.len() >= 2 && is_sep(wb[0]) && is_sep(wb[1]) {
        let after = &work[2..];
        let mut it = after.splitn(3, |c| c == '\\' || c == '/');
        let server = it.next().unwrap_or("");
        let share = it.next().unwrap_or("");
        if !server.is_empty() && !share.is_empty() {
            let remainder = it.next().unwrap_or("");
            return (
                Some(format!("\\\\{}\\{}\\", server, share)),
                split_names(remainder),
            );
        }
    }
    if wb.len() >= 2 && (wb[0] as char).is_ascii_alphabetic() && wb[1] == b':' {
        let drive = format!("{}:", wb[0] as char);
        if wb.len() >= 3 && is_sep(wb[2]) {
            return (Some(format!("{}\\", drive)), split_names(&work[3..]));
        }
        return (Some(drive), split_names(&work[2..]));
    }
    if !wb.is_empty() && is_sep(wb[0]) {
        return (Some("\\".to_string()), split_names(&work[1..]));
    }
    (None, split_names(work))
}

/// Root/name parsing for the `Path` accessor natives, in the **host platform's**
/// path syntax.
///
/// [`p57_parse_win_root`] implements `sun.nio.fs.WindowsPath`'s rules: drive
/// letters, UNC shares, verbatim `\\?\` prefixes. None of those are path syntax
/// on Unix, where `sun.nio.fs.UnixPath` has exactly one root (`/`), no drive
/// concept, `\` is an ordinary filename character, and `//server/share` is just
/// `/server/share`. Running the Windows parser there made
/// `Paths.get("/tmp").getRoot()` report `\` and `Paths.get("//tmp/x")` report a
/// UNC root with zero name elements. Dispatch on the target instead.
///
/// [`p57_normalize_path`]/[`p57_relativize`] deliberately keep using the Windows
/// parser on every target: they are documented as operating on the
/// `/`-canonical internal form as a platform-neutral superset, and their unit
/// tests pin Windows-syntax expectations that run on every host.
#[cfg(windows)]
pub(crate) fn p57_parse_root(s: &str) -> (Option<String>, Vec<String>) {
    p57_parse_win_root(s)
}

#[cfg(not(windows))]
pub(crate) fn p57_parse_root(s: &str) -> (Option<String>, Vec<String>) {
    let split_names = |rest: &str| -> Vec<String> {
        rest.split('/')
            .filter(|seg| !seg.is_empty())
            .map(|seg| seg.to_string())
            .collect()
    };
    if let Some((_tag, _container, entry)) = vfs_decode(s) {
        return (
            Some("/".to_string()),
            split_names(entry.trim_start_matches('/')),
        );
    }
    match s.strip_prefix('/') {
        Some(rest) => (Some("/".to_string()), split_names(rest)),
        None => (None, split_names(s)),
    }
}

/// The name elements a `Path` exposes through `getNameCount`/`getName`/
/// `subpath`/`iterator`.
///
/// This is [`p57_parse_root`]'s name list with one correction: the **empty
/// path** has exactly ONE name element — the empty string — not zero.
/// `sun.nio.fs.UnixPath`/`WindowsPath` both special-case it that way
/// (`Paths.get("").getNameCount()` is 1 on HotSpot and `getName(0)` returns the
/// empty path), because an empty path denotes the default directory and must
/// still be iterable. `p57_parse_root` splits on separators and filters empty
/// segments, so it reports 0, and `getName(0)`/`subpath(0,1)`/`iterator()` then
/// threw `IllegalArgumentException` / yielded nothing where HotSpot hands back
/// the empty name.
///
/// Only the empty string reaches this arm: any other relative input has at
/// least one non-empty segment, and any rooted input reports a root. Kept
/// separate from `p57_parse_root` on purpose — [`p57_normalize_path`] and
/// [`p57_relativize`] must keep seeing zero elements there, since HotSpot's
/// `Paths.get("").relativize(Paths.get("a"))` is `a`, not `../a`.
pub(crate) fn p57_name_elements(path: &str) -> Vec<String> {
    let (root, names) = p57_parse_root(path);
    if root.is_none() && names.is_empty() {
        return vec![String::new()];
    }
    names
}

/// POSIX (`sun.nio.fs.UnixPath`) `getParent()` semantics, the Unix twin of
/// [`p57_win_parent_of`]: a pure last-separator split that keeps `.`/`..` name
/// elements verbatim. Rust's `std::path::Path::parent()` normalizes a trailing
/// `.` away first and so over-trims — the parent of `a/b/.` came back as `a`
/// instead of `a/b`. Returns "" when there is no parent (the caller maps that
/// to `null`).
#[cfg(not(windows))]
pub(crate) fn p57_posix_parent_of(path: &str) -> String {
    let (root, names) = p57_parse_root(path);
    let n = names.len();
    match root {
        // Rooted path (`/a/b`): one element under the root leaves the root
        // itself as the parent; the root already carries its separator.
        Some(r) => {
            if n == 0 {
                String::new()
            } else if n == 1 {
                r
            } else {
                format!("{r}{}", names[..n - 1].join("/"))
            }
        }
        // Relative path (`a/b/c`).
        None => {
            if n <= 1 {
                String::new()
            } else {
                names[..n - 1].join("/")
            }
        }
    }
}

/// keycloak-15: Windows (`sun.nio.fs.WindowsPath`) `isAbsolute()` semantics.
///
/// A Windows path is absolute **only** when it names both a root *and* a drive
/// (or is a UNC path). Two prefixes that have a root component but are NOT
/// absolute trip up a naive "has a root ⇒ absolute" check:
///   * **drive-relative** `C:foo` — relative to the current dir *on drive C*;
///   * **driveless-rooted** `\foo` / `/foo` — relative to the current *drive*.
/// HotSpot returns `false` for both; the previous
/// `p.starts_with('/') || p[1]==':'` heuristic returned `true`. Classify off the
/// parsed root instead (roots are rendered in `\`-form by [`p57_parse_win_root`]):
///   * `C:\` (drive + separator)  → absolute
///   * `\\server\share\` (UNC)    → absolute
///   * `C:` (drive only)          → NOT absolute (drive-relative)
///   * `\`  (separator only)      → NOT absolute (driveless-rooted)
pub(crate) fn p57_win_is_absolute(s: &str) -> bool {
    let is_sep = |c: u8| c == b'\\' || c == b'/';
    match p57_parse_win_root(s).0 {
        None => false,
        Some(root) => {
            let b = root.as_bytes();
            let unc = b.len() >= 2 && is_sep(b[0]) && is_sep(b[1]);
            let drive_abs = b.len() >= 3 && b[1] == b':' && is_sep(b[2]);
            unc || drive_abs
        }
    }
}

/// keycloak-15: Windows (`sun.nio.fs.WindowsPath`) `getParent()` semantics.
///
/// HotSpot computes the parent as a pure last-separator split that keeps `.`/`..`
/// name elements verbatim — it does **not** normalize curdir. Rust's
/// `std::path::Path::parent()` normalizes a trailing `.` away first, so it
/// over-trims: the parent of `C:\a\b\.` becomes `C:\a` instead of `C:\a\b`.
/// Rebuild the parent from the parsed (root, names) so it stays consistent with
/// `getRoot`/`getNameCount`/`getName`. Returns "" when there is no parent
/// (caller maps that to `null`).
pub(crate) fn p57_win_parent_of(path: &str) -> String {
    let (root, names) = p57_parse_win_root(path);
    let n = names.len();
    match root {
        // Rooted path (`C:\…`, `\\server\share\…`, `\…`, `C:foo`).
        Some(r) => {
            if n == 0 {
                // The path is just the root → no parent.
                String::new()
            } else if n == 1 {
                // A single element under a root → the parent is the root itself.
                r
            } else {
                // root + all-but-last name. The root string already carries its
                // trailing separator for absolute/UNC roots; a drive-relative
                // root (`C:`) carries none, so the first name attaches directly —
                // which is exactly HotSpot's `C:foo\bar` → parent `C:foo`.
                let mut out = r;
                for (i, name) in names[..n - 1].iter().enumerate() {
                    if i > 0 {
                        out.push('\\');
                    }
                    out.push_str(name);
                }
                out
            }
        }
        // Relative path (`a\b\c`).
        None => {
            if n <= 1 {
                String::new()
            } else {
                names[..n - 1].join("\\")
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod p57_win_path_tests {
    //! keycloak-15: Windows `WindowsPath.isAbsolute()` / `getParent()` semantics.
    //! Pure-function tests (no VM); each expectation matches HotSpot
    //! `sun.nio.fs.WindowsPath` exactly (cross-checked against JDK 25 via the
    //! `PVerify` repro). The parser accepts both `\` and the `/`-canonical
    //! internal form, so both spellings are exercised.
    use super::{p57_win_is_absolute, p57_win_parent_of};
    #[cfg(windows)]
    use super::p57_trim_path_trailing_separator;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn is_absolute_matches_hotspot() {
        // Absolute: drive + root, or UNC.
        assert!(p57_win_is_absolute("C:\\foo\\bar"));
        assert!(p57_win_is_absolute("C:/foo/bar")); // `/`-canonical internal form
        assert!(p57_win_is_absolute("\\\\server\\share\\d"));
        assert!(p57_win_is_absolute("//server/share/d"));
        // NOT absolute: drive-relative, driveless-rooted, relative.
        assert!(!p57_win_is_absolute("C:foo"));
        assert!(!p57_win_is_absolute("\\foo\\bar"));
        assert!(!p57_win_is_absolute("/foo/bar"));
        assert!(!p57_win_is_absolute("a\\b\\c"));
        assert!(!p57_win_is_absolute("a/b/c"));
        assert!(!p57_win_is_absolute(""));
    }

    #[test]
    fn parent_keeps_curdir_and_root_boundary() {
        // Trailing `.` must be kept (Rust Path::parent would over-trim to "C:\a").
        assert_eq!(p57_win_parent_of("C:\\a\\b\\."), "C:\\a\\b");
        assert_eq!(p57_win_parent_of("C:/a/b/."), "C:\\a\\b");
        // Drive-absolute.
        assert_eq!(p57_win_parent_of("C:\\foo\\bar"), "C:\\foo");
        assert_eq!(p57_win_parent_of("C:\\foo"), "C:\\");
        // Drive-relative: the first name attaches to `C:` with no separator.
        assert_eq!(p57_win_parent_of("C:foo\\bar"), "C:foo");
        assert_eq!(p57_win_parent_of("C:foo"), "C:");
        // Driveless-rooted.
        assert_eq!(p57_win_parent_of("\\foo\\bar"), "\\foo");
        assert_eq!(p57_win_parent_of("\\foo"), "\\");
        // UNC.
        assert_eq!(
            p57_win_parent_of("\\\\server\\share\\d\\e"),
            "\\\\server\\share\\d"
        );
        // Relative.
        assert_eq!(p57_win_parent_of("a\\b\\c"), "a\\b");
        // No parent → "" (caller maps to null).
        assert_eq!(p57_win_parent_of("a"), "");
        assert_eq!(p57_win_parent_of("C:\\"), "");
        assert_eq!(p57_win_parent_of("\\\\server\\share\\"), "");
    }

    #[test]
    #[cfg(windows)]
    fn path_construction_trims_only_non_root_trailing_separators() {
        assert_eq!(p57_trim_path_trailing_separator("a\\"), "a");
        assert_eq!(p57_trim_path_trailing_separator("C:/a/"), "C:/a");
        assert_eq!(p57_trim_path_trailing_separator("C:/"), "C:/");
        assert_eq!(p57_trim_path_trailing_separator("\\\\server\\share\\"), "\\\\server\\share\\");
        assert_eq!(p57_trim_path_trailing_separator("\\"), "\\");
    }
}

#[cfg(test)]
pub(crate) mod p57_name_element_tests {
    //! The empty path's name list, on every host. HotSpot:
    //! `Paths.get("").getNameCount()` is 1 and `getName(0)` is the empty path,
    //! on both `UnixPath` and `WindowsPath`.
    use super::{p57_name_elements, p57_parse_root};

    #[test]
    fn the_empty_path_has_one_empty_name_element() {
        assert_eq!(p57_name_elements(""), vec![String::new()]);
        // …and `p57_parse_root` must keep reporting zero there, because
        // `normalize`/`relativize` depend on it: HotSpot's
        // `Paths.get("").relativize(Paths.get("a"))` is `a`, not `../a`.
        assert!(p57_parse_root("").1.is_empty());
    }

    #[test]
    fn every_other_path_is_unchanged() {
        // Rooted paths report a root, so the empty-name arm cannot fire even
        // when they have no name elements.
        let root = if cfg!(windows) { "C:/" } else { "/" };
        assert!(p57_name_elements(root).is_empty());
        assert_eq!(p57_name_elements("a/b"), vec!["a", "b"]);
        assert_eq!(p57_name_elements("a"), vec!["a"]);
        assert_eq!(p57_name_elements("a/b/"), vec!["a", "b"]);
    }
}

#[cfg(test)]
#[cfg(not(windows))]
pub(crate) mod p57_posix_path_tests {
    //! POSIX (`sun.nio.fs.UnixPath`) construction / root / parent semantics.
    //! Pure-function tests (no VM); every expectation was cross-checked against
    //! the host JDK on Linux (`PathMatrix` repro) — see
    //! `fixed-suite-bugs/springboot/resourcestests-trailing-slash-path-normalization-FIXED-20260804.md`.
    //! The Windows twin lives in `p57_win_path_tests`.
    use super::{
        jarfs_encode, p57_alloc_path, p57_parse_root, p57_posix_parent_of, p57_read_path,
        p57_trim_path_trailing_separator,
    };
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn path_construction_normalizes_like_unixpath() {
        // The defect: an ordinary trailing separator survived into the stored
        // string, so `Files.writeString` got a directory-shaped path (EISDIR).
        assert_eq!(p57_trim_path_trailing_separator("/tmp/a/b/"), "/tmp/a/b");
        assert_eq!(p57_trim_path_trailing_separator("a/b/"), "a/b");
        assert_eq!(p57_trim_path_trailing_separator("a/b///"), "a/b");
        // Runs of `/` collapse, exactly as `normalizeAndCheck` does.
        assert_eq!(p57_trim_path_trailing_separator("//tmp/x"), "/tmp/x");
        assert_eq!(p57_trim_path_trailing_separator("a//b"), "a/b");
        // The root keeps its separator; the empty path stays empty.
        assert_eq!(p57_trim_path_trailing_separator("/"), "/");
        assert_eq!(p57_trim_path_trailing_separator("//"), "/");
        assert_eq!(p57_trim_path_trailing_separator(""), "");
        // `\\` is an ordinary filename character on Unix, never a separator.
        assert_eq!(p57_trim_path_trailing_separator("a\\b\\"), "a\\b\\");
        // Encoded virtual-FS paths are left alone: the trailing `/` there is
        // part of the jar/jrt entry representation.
        let jar = jarfs_encode("/tmp/x.jar", "dir/");
        assert_eq!(p57_trim_path_trailing_separator(&jar), jar);
    }

    #[test]
    fn alloc_path_stores_the_normalized_string() {
        // Pins the WIRING, not just the helper. The defect was that
        // `p57_alloc_path` had no normalization step at all on this platform
        // (`let stored = path.to_string();`), so a helper-only test would have
        // stayed green straight through it. Everything that builds a Path —
        // `Paths.get`, `resolve`, `getParent`, `toAbsolutePath` — funnels here,
        // and the stored string is what the file-IO bridge syscalls with.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let p = p57_alloc_path(&mut ctx, "/tmp/one/two/three/");
        assert_eq!(p57_read_path(&mut ctx, p), "/tmp/one/two/three");
        let root = p57_alloc_path(&mut ctx, "/");
        assert_eq!(p57_read_path(&mut ctx, root), "/");
        let collapsed = p57_alloc_path(&mut ctx, "/tmp//x/");
        assert_eq!(p57_read_path(&mut ctx, collapsed), "/tmp/x");
    }

    #[test]
    fn parse_root_uses_posix_syntax() {
        // Absolute: the one root is `/` (the Windows parser answered `\\`).
        assert_eq!(p57_parse_root("/tmp/x").0.as_deref(), Some("/"));
        assert_eq!(p57_parse_root("/").0.as_deref(), Some("/"));
        assert_eq!(p57_parse_root("/tmp/x").1, vec!["tmp", "x"]);
        assert_eq!(p57_parse_root("/").1.len(), 0);
        // Relative: no root.
        assert_eq!(p57_parse_root("a/b").0, None);
        assert_eq!(p57_parse_root("a/b").1, vec!["a", "b"]);
        // A drive letter is NOT syntax here — `C:` is an ordinary name.
        assert_eq!(p57_parse_root("C:/x").0, None);
        assert_eq!(p57_parse_root("C:/x").1, vec!["C:", "x"]);
        // No UNC either: `//tmp/x` is just `/tmp/x` (2 names, not a share).
        assert_eq!(p57_parse_root("//tmp/x").0.as_deref(), Some("/"));
        assert_eq!(p57_parse_root("//tmp/x").1, vec!["tmp", "x"]);
        // `\` is a filename character, so it never splits a name.
        assert_eq!(p57_parse_root("a\\b").1, vec!["a\\b"]);
    }

    #[test]
    fn parent_keeps_curdir_and_root_boundary() {
        // Trailing `.` must be kept (Rust's Path::parent would over-trim to "a").
        assert_eq!(p57_posix_parent_of("a/b/."), "a/b");
        assert_eq!(p57_posix_parent_of("/a/b/."), "/a/b");
        // Ordinary splits.
        assert_eq!(p57_posix_parent_of("/tmp/a/b"), "/tmp/a");
        assert_eq!(p57_posix_parent_of("a/b/c"), "a/b");
        // A single element under the root leaves the root itself.
        assert_eq!(p57_posix_parent_of("/foo"), "/");
        // No parent -> "" (caller maps to null).
        assert_eq!(p57_posix_parent_of("a"), "");
        assert_eq!(p57_posix_parent_of("/"), "");
        assert_eq!(p57_posix_parent_of(""), "");
    }
}

#[cfg(test)]
pub(crate) mod p57_normalize_relativize_tests {
    //! `Path.normalize()` / `Path.relativize()` vs HotSpot (JDK 25, via the
    //! `PathDeep` repro). Helpers emit `/`-canonical internal form.
    use super::{p57_normalize_path, p57_relativize};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    #[cfg(windows)]
    fn normalize_preserves_root_and_leading_dotdot() {
        // `..` must not pop above the root.
        assert_eq!(p57_normalize_path("C:/a/../../b"), "C:/b");
        assert_eq!(p57_normalize_path("C:/.."), "C:/");
        assert_eq!(p57_normalize_path("//s/sh/a/.."), "//s/sh/");
        // Leading `..` on a relative path is kept.
        assert_eq!(p57_normalize_path("../../a"), "../../a");
        assert_eq!(p57_normalize_path("a/../../b"), "../b");
        // Ordinary collapses.
        assert_eq!(p57_normalize_path("C:/a/./b/.."), "C:/a");
        assert_eq!(p57_normalize_path("a/./b/.."), "a");
        assert_eq!(p57_normalize_path("a/../b"), "b");
        assert_eq!(p57_normalize_path("C:/a/b"), "C:/a/b");
        // Drive-relative root retained with no separator before the first name.
        assert_eq!(p57_normalize_path("C:a/../b"), "C:b");
    }

    #[test]
    #[cfg(windows)]
    fn relativize_backtracks_with_dotdot() {
        assert_eq!(p57_relativize("C:/a/b", "C:/a/x").as_deref(), Some("../x"));
        assert_eq!(p57_relativize("a/b/c", "a/b").as_deref(), Some(".."));
        assert_eq!(
            p57_relativize("C:/a/b", "C:/a/b/c/d").as_deref(),
            Some("c/d")
        );
        assert_eq!(p57_relativize("C:/a", "C:/a").as_deref(), Some(""));
        assert_eq!(p57_relativize("a/b", "a/b/c").as_deref(), Some("c"));
        // Case-insensitive common-prefix match on Windows.
        #[cfg(windows)]
        assert_eq!(p57_relativize("C:/A/b", "C:/a/x").as_deref(), Some("../x"));
        // Different roots → None (caller falls back to target).
        assert_eq!(p57_relativize("C:/a", "D:/b"), None);
        assert_eq!(p57_relativize("C:/a", "rel/b"), None);
    }

    /// The POSIX twins. These are the same two functions on a host where a
    /// drive prefix and a backslash are NOT path syntax; every expectation was
    /// cross-checked against the host JDK 25 on Linux (`NormMatrix` repro).
    #[test]
    #[cfg(not(windows))]
    fn normalize_preserves_root_and_leading_dotdot_posix() {
        // `..` must not pop above the root.
        assert_eq!(p57_normalize_path("/a/../../b"), "/b");
        assert_eq!(p57_normalize_path("/.."), "/");
        // Leading `..` on a relative path is kept.
        assert_eq!(p57_normalize_path("../../a"), "../../a");
        assert_eq!(p57_normalize_path("a/../../b"), "../b");
        // Ordinary collapses.
        assert_eq!(p57_normalize_path("/a/./b/.."), "/a");
        assert_eq!(p57_normalize_path("a/./b/.."), "a");
        assert_eq!(p57_normalize_path("a/../b"), "b");
        assert_eq!(p57_normalize_path("/a/b"), "/a/b");
        // A drive prefix is an ordinary NAME here, so `..` cancels it and the
        // Windows parser's "root" reading (`C:b`) is wrong on this host.
        assert_eq!(p57_normalize_path("C:a/../b"), "b");
        assert_eq!(p57_normalize_path("C:/a/../../b"), "b");
        // `\` is a filename character: `a\b` is ONE name, so the `..` that
        // follows cancels the whole thing (the Windows parser answered `a/c`).
        assert_eq!(p57_normalize_path("a\\b/../c"), "c");
        // No UNC: `//s/sh/a/..` is just `/s/sh`.
        assert_eq!(p57_normalize_path("//s/sh/a/.."), "/s/sh");
    }

    #[test]
    #[cfg(not(windows))]
    fn relativize_backtracks_with_dotdot_posix() {
        assert_eq!(p57_relativize("/a/b", "/a/x").as_deref(), Some("../x"));
        assert_eq!(p57_relativize("a/b/c", "a/b").as_deref(), Some(".."));
        assert_eq!(p57_relativize("/a/b", "/a/b/c/d").as_deref(), Some("c/d"));
        assert_eq!(p57_relativize("/a", "/a").as_deref(), Some(""));
        assert_eq!(p57_relativize("a/b", "a/b/c").as_deref(), Some("c"));
        // Case-SENSITIVE name matching off Windows.
        assert_eq!(p57_relativize("/A/b", "/a/x").as_deref(), Some("../../a/x"));
        // Drive prefixes are ordinary names, so these ARE relativizable here.
        assert_eq!(
            p57_relativize("C:/a", "D:/b").as_deref(),
            Some("../../D:/b")
        );
        // `\` never splits a name.
        assert_eq!(
            p57_relativize("/a\\b", "/a\\x").as_deref(),
            Some("../a\\x")
        );
        // Absolute vs relative → None (caller falls back to target).
        assert_eq!(p57_relativize("/a", "rel/b"), None);
    }
}

/// True if `path_obj`'s recorded owning FileSystem (P57_PATH_FS_FIELD) is a
/// virtual (mounted-jar or runtime-image) FileSystem. Used by `Path.toString`
/// to render '/' for relative paths that belong to such a FS.
pub(crate) fn path_owned_by_virtual_fs(ctx: &mut dyn NativeContext, path_obj: ObjectRef) -> bool {
    if let Value::Object(Some(fs)) = ctx.get_field(path_obj, P57_PATH_FS_FIELD) {
        matches!(ctx.get_field(fs, P57_FS_JAR_FIELD), Value::Object(Some(_)))
            || matches!(ctx.get_field(fs, P57_FS_JRT_FIELD), Value::Object(Some(_)))
    } else {
        false
    }
}

/// Windows `Path` syntax validation for the `Paths.get` factory — the ONLY
/// p57 entry point that builds a `Path` directly from unvalidated caller
/// input (every other `p57_alloc_path` call site here derives its string
/// from an already-validated `Path`, e.g. `getParent`/`resolve`/`normalize`).
/// A colon is only legal as the second character of a drive specifier
/// (`C:...`) — anywhere else (including a bare `scheme:rest` string like
/// Spring's `ping:foo` `ProtocolResolver` probe) it's illegal, matching real
/// `sun.nio.fs.WindowsPathParser`. Mirrors `native-io`'s
/// `validate_windows_path` (duplicated rather than shared: this is a
/// separate crate, and this specific "mixed real-JDK mode" registration
/// (`register_phase57_nio_file`) runs AFTER — and overwrites — `native-io`'s
/// `Paths.get` registration for the same method key).
pub(crate) fn p57_validate_windows_path(s: &str) -> Result<(), &'static str> {
    if !cfg!(windows) {
        return Ok(());
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 4
        && (bytes[0] == b'\\' || bytes[0] == b'/')
        && (bytes[1] == b'\\' || bytes[1] == b'/')
        && bytes[2] == b'?'
    {
        return Ok(());
    }
    let drive_colon = if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(1usize)
    } else {
        None
    };
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' && Some(i) != drive_colon {
            return Err("Illegal char <:>");
        }
        if b < 0x20 || matches!(b, b'<' | b'>' | b'"' | b'|' | b'?' | b'*') {
            return Err("Illegal char");
        }
    }
    Ok(())
}

/// Validate `path` before wrapping it in a synthetic p57 `Path` object;
/// throws a real `java.nio.file.InvalidPathException` (matching real JDK)
/// instead of silently accepting any string. See [`p57_validate_windows_path`].
pub(crate) fn p57_alloc_path_checked(ctx: &mut dyn NativeContext, path: &str) -> MethodCallResult {
    if let Err(reason) = p57_validate_windows_path(path) {
        return match ctx.new_object("java/nio/file/InvalidPathException") {
            Ok(Some(Value::Object(Some(exc)))) => {
                // Pin across the create_strings below — a moving young GC
                // there would relocate the fresh exception (native
                // stale-local family).
                let exc_pin = ctx.pin_native_root(exc);
                let input_str = ctx.create_string(path);
                let input_pin = ctx.pin_native_root(input_str);
                let reason_str = ctx.create_string(reason);
                let exc = ctx.read_native_pin(exc_pin, exc);
                let input_str = ctx.read_native_pin(input_pin, input_str);
                let _ = ctx.invoke(
                    "java/nio/file/InvalidPathException",
                    "<init>",
                    "(Ljava/lang/String;Ljava/lang/String;)V",
                    &[
                        Value::Object(Some(exc)),
                        Value::Object(Some(input_str)),
                        Value::Object(Some(reason_str)),
                    ],
                );
                let exc = ctx.read_native_pin(exc_pin, exc);
                ctx.unpin_native_roots(exc_pin);
                Err(MethodCallFailed::ExceptionThrown(exc))
            }
            _ => Err(RuntimeError::IllegalArgumentException {
                message: format!("{reason}: {path}"),
            }
            .into()),
        };
    }
    Ok(Some(Value::Object(Some(p57_alloc_path(ctx, path)))))
}

pub(crate) fn p57_alloc_path(ctx: &mut dyn NativeContext, path: &str) -> ObjectRef {
    // 2 fields: [0] = path String, [1] = owning FileSystem (P57_PATH_FS_FIELD,
    // null unless set by `FileSystem.getPath`).
    let obj = alloc_concurrent_synthetic(ctx, "java/nio/file/Path", 2);
    // Canonicalise separators to '/' internally so Path operations
    // (normalize/equals/resolve/hashCode) and the file-IO natives all see one
    // form. Windows accepts '\' as a separator and HotSpot's WindowsPath stores
    // '\', but rendering '\' lives at the `toString()`/`getPath()` display
    // boundary (see `Path.toString`); internally we keep '/'. A real Windows
    // filename can never contain '\', so folding it to '/' is loss-free there.
    // On Unix '\' is a legal filename character — leave it untouched. jar-FS
    // encoded strings carry a sentinel + their own '/'-separated entry, so
    // never rewrite those.
    #[cfg(windows)]
    let stored = if jarfs_decode(path).is_some() {
        path.to_string()
    } else {
        p57_trim_path_trailing_separator(path).replace('\\', "/")
    };
    // Unix has one separator and one root, so there is nothing to fold — but
    // the JDK still normalizes at construction (see
    // `p57_trim_path_trailing_separator`), and skipping that was the whole
    // trailing-separator defect.
    #[cfg(not(windows))]
    let stored = p57_trim_path_trailing_separator(path);
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh Path (native stale-local family).
    let obj_pin = ctx.pin_native_root(obj);
    let s = ctx.create_string(&stored);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, P57_PATH_FIELD, Value::Object(Some(s)));
    ctx.unpin_native_roots(obj_pin);
    obj
}

/// The `OpenOption`s the nio open paths act on, as scanned by
/// [`fsp_scan_open_options`].
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct P57OpenFlags {
    /// `StandardOpenOption.APPEND` — open at end-of-file instead of truncating.
    pub append: bool,
    /// `StandardOpenOption.CREATE_NEW` — fail if the file already exists.
    pub create_new: bool,
    /// `LinkOption.NOFOLLOW_LINKS`. It implements `OpenOption` as well as
    /// `CopyOption`, so it is legal in every `open`/`newXStream` varargs list —
    /// see [`p57_nofollow_reject`] for what it obliges us to do.
    pub nofollow: bool,
}

/// Inspect a `Set<OpenOption>` or `OpenOption[]` for APPEND / CREATE_NEW /
/// NOFOLLOW_LINKS.
///
/// `Files.newOutputStream(path, opts...)` packages varargs as an OpenOption[]
/// before dispatching to the provider; the provider's default impl converts
/// the array to a `HashSet<OpenOption>` and then forwards to `newByteChannel`.
/// We need to honour APPEND (open-for-append vs truncate-on-open),
/// CREATE_NEW (fail-if-exists, per spec) and NOFOLLOW_LINKS (refuse a symlink
/// final component) regardless of which container the caller passes us.
/// CREATE/WRITE are implied for an output stream and need no flag.
pub(crate) fn fsp_scan_open_options(
    ctx: &mut dyn NativeContext,
    container: Option<Value>,
) -> P57OpenFlags {
    let mut flags = P57OpenFlags::default();
    let obj = match container {
        Some(Value::Object(Some(o))) => o,
        _ => return flags,
    };
    // Try as array first: array_length returns 0 for non-array objects.
    let arr_len = ctx.array_length(obj);
    if arr_len > 0 {
        for i in 0..arr_len {
            let opt = match ctx.get_array_element(obj, i) {
                Value::Object(Some(o)) => o,
                _ => continue,
            };
            // Enum.name() lives in instance field 0 on synthetic enums.
            let mut name: Option<String> = None;
            if let Value::Object(Some(ns)) = ctx.get_field(opt, 0) {
                name = ctx.read_string(ns);
            }
            // Fallback: invoke toString() on the option.
            if name.is_none() {
                if let Ok(Some(Value::Object(Some(s)))) =
                    ctx.invoke_virtual(opt, "toString", "()Ljava/lang/String;", &[])
                {
                    name = ctx.read_string(s);
                }
            }
            if let Some(n) = name {
                if n.eq_ignore_ascii_case("APPEND") {
                    flags.append = true;
                }
                if n.eq_ignore_ascii_case("CREATE_NEW") {
                    flags.create_new = true;
                }
                if n.eq_ignore_ascii_case("NOFOLLOW_LINKS") {
                    flags.nofollow = true;
                }
            }
        }
        return flags;
    }
    // Set: rely on toString() — `HashSet.toString()` yields `[APPEND, WRITE]`
    // etc. Substring match is robust enough and avoids invoking iterator().
    // NB none of the three tokens is a substring of another StandardOpenOption
    // name (in particular `TRUNCATE_EXISTING` does not contain `CREATE`), so
    // the substring test cannot over-match.
    if let Ok(Some(Value::Object(Some(s)))) =
        ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[])
    {
        if let Some(n) = ctx.read_string(s) {
            let n = n.to_ascii_uppercase();
            flags.append = n.contains("APPEND");
            flags.create_new = n.contains("CREATE_NEW");
            flags.nofollow = n.contains("NOFOLLOW_LINKS");
        }
    }
    flags
}

/// The message HotSpot puts on the `IOException` an `O_NOFOLLOW` open raises.
///
/// Verified against OpenJDK 21 on Linux: `UnixChannelFactory` special-cases
/// `ELOOP` when `NOFOLLOW_LINKS` was requested and throws a **plain
/// `java.io.IOException`** — not a `FileSystemException` — carrying the errno
/// string with this suffix and *no* path prefix. Callers assert on the type
/// (`assertThatIOException`), so the type is what matters; the text is here so
/// a diagnostic log reads the same on both VMs.
pub(crate) const P57_NOFOLLOW_ELOOP_MESSAGE: &str =
    "Too many levels of symbolic links (NOFOLLOW_LINKS specified)";

/// Enforce `LinkOption.NOFOLLOW_LINKS` on an open: refuse when the **final**
/// component of `path` is itself a symbolic link.
///
/// The platform providers implement this by adding `O_NOFOLLOW` to the open
/// flags, so the kernel fails the open with `ELOOP` *before* anything is
/// created or truncated. That distinction is the whole point of the option:
/// `ApplicationPid.write` uses it so a PID file that an attacker has replaced
/// with a link cannot be used to write through to the link's target. Following
/// the link "successfully" is exactly the outcome the caller asked us to
/// prevent, and it is silent.
///
/// We approximate `O_NOFOLLOW` with an `lstat` (`symlink_metadata`, which does
/// NOT resolve the final component) taken immediately before the open. Only the
/// last component is inspected — `NOFOLLOW_LINKS` says nothing about symlinks
/// higher up the path, and the JDK likewise happily writes to
/// `<symlinked-dir>/file`.
///
/// Returns `Some(exception)` when the open must be refused, `None` when it may
/// proceed. A path that does not exist at all is *not* refused here: an
/// `O_NOFOLLOW|O_CREAT` open of a missing name succeeds, and only a **dangling
/// symlink** — which `symlink_metadata` reports as a symlink even though
/// `Path::exists()` (a `stat`) says the path is absent — still `ELOOP`s.
pub(crate) fn p57_nofollow_reject(path: &str) -> Option<MethodCallFailed> {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return None;
    }
    Some(
        RuntimeError::IOException {
            message: P57_NOFOLLOW_ELOOP_MESSAGE.to_string(),
        }
        .into(),
    )
}

/// `Files.newInputStream` / `FileSystemProvider.newInputStream` — a **lazy**
/// stream over `path`, the mirror image of [`fsp_new_output_stream`].
///
/// These used to read the whole file up front and hand back a
/// `ByteArrayInputStream` over a snapshot. That is wasteful for ordinary files
/// (a full copy of anything anyone streams) and *fatal* for anything that is
/// not a regular file: `std::fs::read` on a character device never returns.
/// Lucene's `org.apache.lucene.util.StringHelper.<clinit>` does
///
/// ```java
/// new DataInputStream(Files.newInputStream(Paths.get("/dev/urandom"))).readLong()
/// ```
///
/// — it wants eight bytes. Slurping `/dev/urandom` instead grew an unbounded
/// `Vec<u8>` at ~600 MB/s until the kernel OOM-killed the process. Nothing
/// bounded it: this is native memory, so `-Xmx` is irrelevant (peak RSS was
/// ~14 GB at every heap size from 128m to 4g), and the kernel's SIGKILL left
/// no Java exception and no VM diagnostic behind. See
/// docs/known-issues/h2/bug-filechannel-map-anon-memory-growth.md.
///
/// `path_index` is where the `Path` argument sits: 0 for the `Files` statics,
/// 1 for the `FileSystemProvider` instance method (whose slot 0 is `this`).
pub(crate) fn fsp_new_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    path_index: usize,
) -> MethodCallResult {
    let path_obj = obj_arg(args, path_index)?;
    let p = p57_read_path(ctx, path_obj);
    // Jar/VFS entries have no file descriptor to hand out: they are already
    // decoded in memory and bounded by the entry size, so a snapshot is both
    // correct and the only option there.
    if let Some(read) = vfs_read(&p) {
        return match read {
            Ok(data) => files_byte_array_input_stream(ctx, &data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(p57_no_such_file(ctx, &p)),
            Err(e) => Err(p57_io_error(&e)),
        };
    }
    // NOFOLLOW_LINKS is legal on the READ side too (`O_RDONLY|O_NOFOLLOW` still
    // ELOOPs), and callers use it to be sure they read the file they named and
    // not whatever a link now points at. Options sit one slot past the Path.
    if fsp_scan_open_options(ctx, args.get(path_index + 1).copied()).nofollow {
        if let Some(refused) = p57_nofollow_reject(&p) {
            return Err(refused);
        }
    }
    // GAP I2 — see `newFileChannel`. The check runs before the fd is reserved
    // and before `open`, so a refusal leaves nothing behind.
    let fd = match crate::capability_gate::open_read_gated(&*ctx, &p) {
        Ok(fd) => fd,
        // A refusal is a `SecurityException`: it is not an I/O condition and
        // must not be mapped onto `NoSuchFileException`/`AccessDeniedException`,
        // which callers legitimately catch and recover from.
        Err(cratonvm_native_api::fd_table::FdCapabilityError::Denied(denied)) => {
            return Err(denied.into())
        }
        Err(cratonvm_native_api::fd_table::FdCapabilityError::Io(e)) => {
            return Err(match e.kind() {
                std::io::ErrorKind::NotFound => p57_no_such_file(ctx, &p),
                std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, &p),
                _ => p57_io_error(&e),
            })
        }
    };
    // Wire the fd onto a real `FileInputStream`, filling in every field its
    // constructor would have. `FileInputStream.<init>` is itself natively
    // intercepted (native-io's `native_fis_open0`), so the instance
    // initialiser that creates `closeLock` never runs on any path — which is
    // why native-io has `fis_backfill_constructor_fields` doing exactly this.
    // Miss `closeLock` and the JDK's `close()`, which opens with
    // `synchronized (closeLock)`, NPEs on every try-with-resources.
    let stream = alloc_concurrent_synthetic(ctx, "java/io/FileInputStream", 4);
    // Pin across the allocations below — each can trigger a moving young GC
    // that relocates the fresh stream (native stale-local family).
    let stream_pin = ctx.pin_native_root(stream);
    let fd_obj = ctx.new_object("java/io/FileDescriptor");
    let path_str = ctx.create_string(&p);
    let close_lock = ctx.new_object("java/lang/Object");
    let stream = ctx.read_native_pin(stream_pin, stream);
    ctx.unpin_native_roots(stream_pin);
    let Ok(Some(Value::Object(Some(fd_obj)))) = fd_obj else {
        let _ = ctx.fd_table().close(fd);
        return Err(RuntimeError::IOException {
            message: format!("newInputStream({p}): could not allocate a FileDescriptor"),
        }
        .into());
    };
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd as i32));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(fd as i64));
    ctx.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    // `FileInputStream.path` backs `getChannel()`/diagnostics on the real JDK.
    ctx.set_field_by_name(stream, "path", Value::Object(Some(path_str)));
    if let Ok(Some(lock @ Value::Object(Some(_)))) = close_lock {
        ctx.set_field_by_name(stream, "closeLock", lock);
    }
    ctx.set_field_by_name(stream, "closed", Value::Int(0));
    // Belt-and-braces for legacy callers that read instance slot 0 directly.
    ctx.set_field(stream, 0, Value::Object(Some(fd_obj)));
    Ok(Some(Value::Object(Some(stream))))
}

/// A `ByteArrayInputStream` over `data` — the in-memory stream shape used for
/// VFS (jar) entries, which have no file descriptor to stream from.
fn files_byte_array_input_stream(ctx: &mut dyn NativeContext, data: &[u8]) -> MethodCallResult {
    let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
    // Pin across the array alloc below — a moving young GC there would
    // relocate the fresh stream (native stale-local family).
    let stream_pin = ctx.pin_native_root(stream);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, data.len());
    let stream = ctx.read_native_pin(stream_pin, stream);
    ctx.unpin_native_roots(stream_pin);
    ctx.write_byte_array_from(arr, 0, data);
    ctx.set_field_by_name(stream, "buf", Value::Object(Some(arr)));
    ctx.set_field_by_name(stream, "pos", Value::Int(0));
    ctx.set_field_by_name(stream, "mark", Value::Int(0));
    ctx.set_field_by_name(stream, "count", Value::Int(data.len() as i32));
    Ok(Some(Value::Object(Some(stream))))
}

/// FileSystemProvider.newOutputStream — opens `path` for writing via
/// `fd_table` and returns a `java.io.FileOutputStream` whose `FileDescriptor`
/// carries the fd. See the registration site (above, near `fsp` block) for
/// the rationale; this helper does the actual work and is split out so the
/// closure stays tight enough for the registry macro.
pub(crate) fn fsp_new_output_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path_obj = obj_arg(args, 1)?;
    let p = p57_read_path(ctx, path_obj);
    if p.is_empty() {
        return Err(RuntimeError::IOException {
            message: "newOutputStream: null path".to_string(),
        }
        .into());
    }
    let flags = fsp_scan_open_options(ctx, args.get(2).copied());
    let (append, create_new) = (flags.append, flags.create_new);
    // NOFOLLOW_LINKS must refuse a symlink final component BEFORE the open —
    // otherwise we follow the link and truncate/overwrite its target, which is
    // the precise outcome the option exists to prevent.
    if flags.nofollow {
        if let Some(refused) = p57_nofollow_reject(&p) {
            return Err(refused);
        }
    }
    if create_new && std::path::Path::new(&p).exists() {
        return Err(p57_file_already_exists(ctx, &p));
    }
    // GAP I2 — see `newFileChannel`.
    let fd = match crate::capability_gate::open_write_gated(&*ctx, &p, append) {
        Ok(fd) => fd,
        // A refusal is a `SecurityException`, not one of the typed
        // `java.nio.file` I/O exceptions below.
        Err(cratonvm_native_api::fd_table::FdCapabilityError::Denied(denied)) => {
            return Err(denied.into())
        }
        Err(cratonvm_native_api::fd_table::FdCapabilityError::Io(e)) => {
            // Surface the TYPED `java.nio.file` exception HotSpot throws, not a
            // bare IOException. Opening a directory for output denies access on
            // Windows (os error 5) → `AccessDeniedException` (SC-resource-io
            // Cause C: Spring's `PathResourceTests.getOutputStreamForDirectory`);
            // a missing parent → `NoSuchFileException`. Both are `IOException`
            // subclasses so existing `catch (IOException)` callers are unaffected.
            return Err(match e.kind() {
                std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, &p),
                std::io::ErrorKind::NotFound => p57_no_such_file(ctx, &p),
                _ => RuntimeError::IOException {
                    message: format!("newOutputStream({}): {}", p, e),
                }
                .into(),
            });
        }
    };
    // Allocate a real FileOutputStream and wire the fd onto its
    // FileDescriptor — the existing FOS native overrides (write/flush/close,
    // registered in native-io::lib.rs) recover the fd via the same
    // `fd`/`handle` fields on the FileDescriptor object.
    let fos = alloc_concurrent_synthetic(ctx, "java/io/FileOutputStream", 4);
    // Pin across the FileDescriptor alloc below — a moving young GC there
    // would relocate the fresh stream (native stale-local family).
    let fos_pin = ctx.pin_native_root(fos);
    let fd_obj = alloc_concurrent_synthetic(ctx, "java/io/FileDescriptor", 4);
    let fos = ctx.read_native_pin(fos_pin, fos);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd as i32));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(fd as i64));
    ctx.set_field_by_name(fos, "fd", Value::Object(Some(fd_obj)));
    // Belt-and-braces for legacy callers that read instance slot 0 directly.
    ctx.set_field(fos, 0, Value::Object(Some(fd_obj)));
    ctx.unpin_native_roots(fos_pin);
    Ok(Some(Value::Object(Some(fos))))
}

/// `Files.write(Path, byte[], OpenOption...)`.
///
/// Registered twice (here and in `register_p71_files_bridge`) for the same
/// (class, name, descriptor); registration is last-writer-wins, so both point
/// at this one function and it no longer matters which runs last.
pub(crate) fn files_write_bytes_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path_obj = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    // Reading a byte[] out element-by-element does not allocate, so nothing
    // moves between here and `p57_files_write_bytes` (which pins for itself).
    let len = ctx.array_length(arr);
    let bytes: Vec<u8> = (0..len)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v as u8,
            _ => 0,
        })
        .collect();
    p57_files_write_bytes(ctx, path_obj, &bytes, args.get(2).copied())
}

/// `Files.writeString(Path, CharSequence, OpenOption...)`.
///
/// Split out of the registration closure because the non-`String`
/// `CharSequence` case needs a re-entrant `toString()`, and everything held
/// across it has to be pinned.
pub(crate) fn files_write_string_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path_obj = obj_arg(args, 0)?;
    let content_ref = obj_arg(args, 1)?;
    let options = args.get(2).copied();
    // Fast path: a real String needs no re-entrant call, so nothing can move.
    if let Some(content) = ctx.read_string(content_ref) {
        return p57_files_write_bytes(ctx, path_obj, content.as_bytes(), options);
    }
    // A `CharSequence` need not be a `String` — `StringBuilder` and
    // `CharBuffer` are the common other cases, and this used to
    // `unwrap_or_default()` them into an EMPTY file with no error. `toString()`
    // re-enters and can move both the `Path` we return and the `OpenOption[]`
    // we still have to scan, so pin the pair across it.
    let opts_obj = match options {
        Some(Value::Object(Some(o))) => Some(o),
        _ => None,
    };
    let path_pin = ctx.pin_native_root(path_obj);
    let opts_pin = opts_obj.map(|o| ctx.pin_native_root(o));
    let content = match ctx.invoke_virtual(content_ref, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let path_obj = ctx.read_native_pin(path_pin, path_obj);
    let options = match (opts_obj, opts_pin) {
        (Some(o), Some(h)) => Some(Value::Object(Some(ctx.read_native_pin(h, o)))),
        _ => options,
    };
    ctx.unpin_native_roots(path_pin);
    p57_files_write_bytes(ctx, path_obj, content.as_bytes(), options)
}

/// Shared back end for the `Files.write` / `Files.writeString` statics.
///
/// Each of those was `std::fs::write(&p, bytes)`, which
///
/// * ignored **every** `OpenOption` — `APPEND` truncated instead of appending,
///   `CREATE_NEW` silently overwrote, and `NOFOLLOW_LINKS` wrote straight
///   through a symbolic link to its target;
/// * bypassed the capability gate (GAP I2), leaving the whole `Files.write`
///   surface invisible to any installed path policy; and
/// * reported failure as `IllegalStateException`, which is **not** an
///   `IOException` — so the `catch (IOException)` that callers of a method
///   declared `throws IOException` write could never match it, and neither
///   could AssertJ's `assertThatIOException`.
///
/// Routing through the same gated open `newOutputStream` already used fixes all
/// three at once. See
/// fixed-suite-bugs/springboot/nio-write-ignores-nofollow-links-symlink-20260804-FIXED.md.
pub(crate) fn p57_files_write_bytes(
    ctx: &mut dyn NativeContext,
    path_obj: ObjectRef,
    bytes: &[u8],
    options: Option<Value>,
) -> MethodCallResult {
    let p = p57_read_path(ctx, path_obj);
    // The option scan can fall back to `toString()`, which allocates and may
    // therefore move `path_obj` (this method returns it).
    let path_pin = ctx.pin_native_root(path_obj);
    let flags = fsp_scan_open_options(ctx, options);
    let path_obj = ctx.read_native_pin(path_pin, path_obj);
    ctx.unpin_native_roots(path_pin);

    if flags.nofollow {
        if let Some(refused) = p57_nofollow_reject(&p) {
            return Err(refused);
        }
    }
    if flags.create_new && std::path::Path::new(&p).exists() {
        return Err(p57_file_already_exists(ctx, &p));
    }
    // GAP I2 — see `newFileChannel`.
    let fd = match crate::capability_gate::open_write_gated(&*ctx, &p, flags.append) {
        Ok(fd) => fd,
        // A refusal is a `SecurityException`, not one of the typed
        // `java.nio.file` I/O exceptions.
        Err(cratonvm_native_api::fd_table::FdCapabilityError::Denied(denied)) => {
            return Err(denied.into())
        }
        Err(cratonvm_native_api::fd_table::FdCapabilityError::Io(e)) => {
            return Err(match e.kind() {
                std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, &p),
                std::io::ErrorKind::NotFound => p57_no_such_file(ctx, &p),
                _ => p57_io_error(&e),
            })
        }
    };
    let mut outcome = ctx.fd_table().write_bytes(fd, bytes);
    if outcome.is_ok() {
        outcome = ctx.fd_table().flush(fd);
    }
    // Close regardless: `Files.write` is a complete open-write-close, and a
    // leaked fd here would strand the file handle for the rest of the run.
    let _ = ctx.fd_table().close(fd);
    match outcome {
        Ok(()) => Ok(Some(Value::Object(Some(path_obj)))),
        Err(e) => Err(p57_io_error(&e)),
    }
}

/// Build a *typed* `java.nio.file.NoSuchFileException` for `path` and return it
/// wrapped as a thrown Java exception.
///
/// Using `alloc_concurrent_synthetic` resolves the real
/// `java.nio.file.NoSuchFileException` class, so the thrown object carries the
/// genuine `ClassId` — exception handlers that `catch (NoSuchFileException)` (or
/// any superclass: `FileSystemException`, `IOException`, `Exception`) match it
/// correctly. We populate the `FileSystemException.file` slot (carries the
/// offending path); `FileSystemException.getMessage()` builds the human
/// message from that field, so we deliberately leave `Throwable.detailMessage`
/// null to avoid a doubled `<path>: <path>` message.
pub(crate) fn p57_no_such_file(ctx: &mut dyn NativeContext, path: &str) -> MethodCallFailed {
    let exc = alloc_concurrent_synthetic(ctx, "java/nio/file/NoSuchFileException", 4);
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh exception (native stale-local family).
    let exc_pin = ctx.pin_native_root(exc);
    let file_str = ctx.create_string(path);
    let exc = ctx.read_native_pin(exc_pin, exc);
    // FileSystemException stores the offending path in its `file` field.
    ctx.set_field_by_name(exc, "file", Value::Object(Some(file_str)));
    ctx.unpin_native_roots(exc_pin);
    MethodCallFailed::ExceptionThrown(exc)
}

/// Build a *typed* `java.nio.file.AccessDeniedException` for `path` (mirrors
/// [`p57_no_such_file`]). `AccessDeniedException extends FileSystemException
/// extends IOException`, so the thrown object carries the genuine `ClassId` and
/// matches `catch (AccessDeniedException)` / `FileSystemException` / `IOException`.
/// Used when an OS open returns access-denied (e.g. opening a directory for
/// output on Windows) so the thrown type matches HotSpot.
pub(crate) fn p57_access_denied(ctx: &mut dyn NativeContext, path: &str) -> MethodCallFailed {
    let exc = alloc_concurrent_synthetic(ctx, "java/nio/file/AccessDeniedException", 4);
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh exception (native stale-local family).
    let exc_pin = ctx.pin_native_root(exc);
    let file_str = ctx.create_string(path);
    let exc = ctx.read_native_pin(exc_pin, exc);
    ctx.set_field_by_name(exc, "file", Value::Object(Some(file_str)));
    ctx.unpin_native_roots(exc_pin);
    MethodCallFailed::ExceptionThrown(exc)
}

/// Remove a single filesystem entry, choosing `remove_dir` vs `remove_file` the
/// way the JDK's `Files.delete` does: on the entry's OWN type.
///
/// This used to branch on `Path::is_dir()`, which resolves symbolic links — so
/// deleting a link to a directory called `remove_dir` on the link, which fails
/// with ENOTDIR and (because the error was discarded) left the link on disk.
/// `FileSystemUtils.deleteRecursively` on such a link therefore silently did
/// nothing, stranding it for every later caller.
pub(crate) fn p57_delete_path(path: &str) {
    let is_real_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    if is_real_dir {
        let _ = std::fs::remove_dir(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Build a *typed* `java.nio.file.NotLinkException` for `path` (mirrors
/// [`p57_no_such_file`]). Thrown by `readSymbolicLink` when the path exists but
/// is not a symbolic link — the JDK's contract, and what
/// `Files.readSymbolicLink`'s callers catch.
pub(crate) fn p57_not_link(ctx: &mut dyn NativeContext, path: &str) -> MethodCallFailed {
    let exc = alloc_concurrent_synthetic(ctx, "java/nio/file/NotLinkException", 4);
    let exc_pin = ctx.pin_native_root(exc);
    let file_str = ctx.create_string(path);
    let exc = ctx.read_native_pin(exc_pin, exc);
    ctx.set_field_by_name(exc, "file", Value::Object(Some(file_str)));
    ctx.unpin_native_roots(exc_pin);
    MethodCallFailed::ExceptionThrown(exc)
}

/// Build a *typed* `java.nio.file.FileSystemException` carrying the JDK's
/// three-part `(file, other, reason)` shape. `getMessage()` on the real class
/// assembles `"<file> -> <other>: <reason>"` from exactly these fields, so we
/// leave `Throwable.detailMessage` null (same reasoning as
/// [`p57_no_such_file`]).
///
/// This is the type the JDK raises for an OS-level link failure that is not one
/// of the specific subclasses — most visibly Windows' `ERROR_PRIVILEGE_NOT_HELD`
/// ("A required privilege is not held by the client"), which is what a symlink
/// creation gets on any Windows host without Developer Mode or an elevated
/// token. Reporting it as `UnsupportedOperationException` (the old behaviour)
/// made a *host* limitation look like a missing JDK feature.
pub(crate) fn p57_filesystem_exception(
    ctx: &mut dyn NativeContext,
    file: &str,
    other: Option<&str>,
    reason: &str,
) -> MethodCallFailed {
    let exc = alloc_concurrent_synthetic(ctx, "java/nio/file/FileSystemException", 4);
    let exc_pin = ctx.pin_native_root(exc);
    let file_str = ctx.create_string(file);
    let exc = ctx.read_native_pin(exc_pin, exc);
    ctx.set_field_by_name(exc, "file", Value::Object(Some(file_str)));
    if let Some(other) = other {
        let other_str = ctx.create_string(other);
        let exc = ctx.read_native_pin(exc_pin, exc);
        ctx.set_field_by_name(exc, "other", Value::Object(Some(other_str)));
    }
    let reason_str = ctx.create_string(reason);
    let exc = ctx.read_native_pin(exc_pin, exc);
    ctx.set_field_by_name(exc, "reason", Value::Object(Some(reason_str)));
    ctx.unpin_native_roots(exc_pin);
    MethodCallFailed::ExceptionThrown(exc)
}

/// Windows error code for "a required privilege is not held by the client",
/// which `CreateSymbolicLinkW` returns unless the process token holds
/// `SeCreateSymbolicLinkPrivilege` (administrator) or the machine is in
/// Developer Mode. Rust surfaces it as an uncategorised `io::Error`, so the
/// raw OS code is the only reliable discriminator.
#[cfg(windows)]
const WIN_ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

/// Map an `io::Error` from a link syscall onto the exception type the JDK
/// raises for it. `link` is the path being created/read (the `file` slot);
/// `other` is the second path a two-path operation names, if any.
fn p57_link_io_error(
    ctx: &mut dyn NativeContext,
    e: &std::io::Error,
    link: &str,
    other: Option<&str>,
) -> MethodCallFailed {
    match e.kind() {
        std::io::ErrorKind::AlreadyExists => p57_file_already_exists(ctx, link),
        std::io::ErrorKind::NotFound => p57_no_such_file(ctx, link),
        std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, link),
        _ => {
            #[cfg(windows)]
            if e.raw_os_error() == Some(WIN_ERROR_PRIVILEGE_NOT_HELD) {
                return p57_filesystem_exception(
                    ctx,
                    link,
                    other,
                    "A required privilege is not held by the client",
                );
            }
            // Strip Rust's trailing " (os error N)" so the reason reads like
            // the JDK's, which carries only the system message text.
            let text = e.to_string();
            let reason = text.split(" (os error ").next().unwrap_or(&text).to_string();
            p57_filesystem_exception(ctx, link, other, &reason)
        }
    }
}

/// Create a real symbolic link at `link` pointing at `target`.
///
/// `target` is stored verbatim — a relative target must stay relative, because
/// that is what `readSymbolicLink` has to hand back and what makes a Kubernetes
/// ConfigMap tree (`..data/<key>` plus one relative link per key) resolve.
pub(crate) fn p57_create_symbolic_link(
    ctx: &mut dyn NativeContext,
    link: &str,
    target: &str,
) -> Result<(), MethodCallFailed> {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = {
        // Windows needs to know at creation time whether the link is a file or
        // a directory link — there is no "don't care" flag. The JDK decides the
        // same way: it reads the target's attributes (resolving a relative
        // target against the link's own parent) and sets
        // SYMBOLIC_LINK_FLAG_DIRECTORY when it is a directory. A dangling
        // target is unknowable, so it falls back to a file link, as the JDK's
        // `WindowsLinkSupport` does.
        let resolved = if std::path::Path::new(target).is_absolute() {
            std::path::PathBuf::from(target)
        } else {
            std::path::Path::new(link)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(target)
        };
        let target_is_dir = std::fs::metadata(&resolved)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        // `symlink_file`/`symlink_dir` do NOT report an existing link as
        // AlreadyExists on every Windows build, so pre-check to keep the
        // `FileAlreadyExistsException` contract identical on both platforms.
        if std::fs::symlink_metadata(link).is_ok() {
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "link already exists",
            ))
        } else if target_is_dir {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) => Err(p57_link_io_error(ctx, &e, link, Some(target))),
    }
}

/// Create a hard link at `link` referring to the same file as `existing`.
pub(crate) fn p57_create_hard_link(
    ctx: &mut dyn NativeContext,
    link: &str,
    existing: &str,
) -> Result<(), MethodCallFailed> {
    match std::fs::hard_link(existing, link) {
        Ok(()) => Ok(()),
        // `hard_link` reports a missing SOURCE as NotFound too, but the JDK
        // names the link being created in either case, so the generic mapping
        // is right.
        Err(e) => Err(p57_link_io_error(ctx, &e, link, Some(existing))),
    }
}

/// Read the target of the symbolic link at `path`, as a fresh `Path`.
///
/// Handles the runtime-image (jrt) package links first — those are synthesised
/// out of the jimage, not the host filesystem — then falls through to a real
/// `readlink(2)`/`DeviceIoControl` read.
pub(crate) fn p57_read_symbolic_link(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> MethodCallResult {
    if let Some((java_home, entry)) = jrtfs_decode(path) {
        if let Some(target) = entry.strip_prefix("packages/").and_then(|rest| {
            jrt_image(&java_home).and_then(|image| jrt_package_link_target(&image, rest))
        }) {
            let p = p57_alloc_path(ctx, &jrtfs_encode(&java_home, &format!("/{target}")));
            return Ok(Some(Value::Object(Some(p))));
        }
        return Err(p57_no_such_file(ctx, path));
    }
    // Distinguish "missing" from "present but not a link" before reading, so
    // both map to the JDK's types (NoSuchFileException vs NotLinkException)
    // rather than to whatever errno `readlink` happens to produce for each.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_symlink() => return Err(p57_not_link(ctx, path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(p57_no_such_file(ctx, path))
        }
        _ => {}
    }
    match std::fs::read_link(path) {
        Ok(target) => {
            let target = target.to_string_lossy().into_owned();
            Ok(Some(Value::Object(Some(p57_alloc_path(ctx, &target)))))
        }
        Err(e) => Err(p57_link_io_error(ctx, &e, path, None)),
    }
}

// --- jar-filesystem path encoding ---------------------------------------
//
// `FileSystemProvider.newFileSystem(Path jar, Map)` mounts the interior of a
// JAR as a `FileSystem`.  We model that filesystem's `Path`s as strings of the
// form  `\x01JARFS\x01<jar-os-path>\x01<entry-within-jar>`.  The leading
// `\x01` sentinel cannot occur in a real path, so the file-IO natives can
// detect jar-FS paths and read the entry straight out of the archive instead
// of going to the host filesystem (which would `ENOENT`).
pub(crate) const JARFS_SENTINEL: char = '\u{1}';

/// Encoded-virtual-filesystem tags. Both the jar-FS and the runtime-image
/// (jrt) FS encode their `Path`s as sentinel-delimited strings of the form
/// `\x01<TAG>\x01<container>\x01<entry>` so the file-IO natives can detect
/// them and read the entry out of the archive / jimage instead of the host
/// filesystem. The TAG distinguishes the backing store: `JARFS` →
/// `<container>` is the jar's OS path (read via the `zip` crate); `JRTFS` →
/// `<container>` is `java.home` and `<entry>` is a jrt logical path like
/// `modules/java.base/java/lang` (read via the jimage `lib/modules`).
pub(crate) const JARFS_TAG: &str = "JARFS";

pub(crate) const JRTFS_TAG: &str = "JRTFS";

/// Generic encoder for the sentinel-delimited virtual-FS path form.
pub(crate) fn vfs_encode(tag: &str, container: &str, entry: &str) -> String {
    let entry = entry.trim_start_matches('/');
    format!("{JARFS_SENTINEL}{tag}{JARFS_SENTINEL}{container}{JARFS_SENTINEL}{entry}")
}

/// Generic decoder: returns `(tag, container, entry)` for any recognised
/// virtual-FS path, or `None` for a plain host path. The tag is mapped to one
/// of the `&'static` tag constants so callers can compare it by identity/value.
pub(crate) fn vfs_decode(p: &str) -> Option<(&'static str, String, String)> {
    let body = p.strip_prefix(JARFS_SENTINEL)?;
    let t_end = body.find(JARFS_SENTINEL)?;
    let tag = &body[..t_end];
    let rest = &body[t_end + JARFS_SENTINEL.len_utf8()..];
    // The container may itself be a jar-FS path when a `jar:nested:` URI
    // mounts an archive stored inside another archive.  Its encoding therefore
    // contains sentinel delimiters of its own; the final delimiter is the only
    // boundary that unambiguously separates the outer container from this
    // path's entry.
    let c_end = rest.rfind(JARFS_SENTINEL)?;
    let container = rest[..c_end].to_string();
    let entry = rest[c_end + JARFS_SENTINEL.len_utf8()..].to_string();
    let tag: &'static str = match tag {
        t if t == JARFS_TAG => JARFS_TAG,
        t if t == JRTFS_TAG => JRTFS_TAG,
        _ => return None,
    };
    Some((tag, container, entry))
}

pub(crate) fn jarfs_encode(jar: &str, entry: &str) -> String {
    vfs_encode(JARFS_TAG, jar, entry)
}

/// If `p` is a jar-FS encoded path, return `(jar_os_path, entry)`.
pub(crate) fn jarfs_decode(p: &str) -> Option<(String, String)> {
    match vfs_decode(p) {
        Some((tag, c, e)) if tag == JARFS_TAG => Some((c, e)),
        _ => None,
    }
}

pub(crate) fn jrtfs_encode(java_home: &str, entry: &str) -> String {
    vfs_encode(JRTFS_TAG, java_home, entry)
}

/// If `p` is a jrt-FS (runtime-image) encoded path, return `(java_home, entry)`
/// where `entry` is the jrt logical path without a leading slash (e.g.
/// `modules/java.base/java/lang`).
pub(crate) fn jrtfs_decode(p: &str) -> Option<(String, String)> {
    match vfs_decode(p) {
        Some((tag, c, e)) if tag == JRTFS_TAG => Some((c, e)),
        _ => None,
    }
}

/// Read a jar-internal entry's bytes. `entry` "" or "/" means the jar root
/// (a directory) — callers should treat that as a directory, not a file.
/// Cache of mounted-jar file bytes, keyed by OS path. The jar-FS helpers
/// (`jarfs_classify`/`jarfs_list_dir`/`jarfs_read_entry`, called once per
/// directory/file during a `Files.walkFileTree`/`Files.walk`) each previously
/// `std::fs::read` the whole archive — O(entries × jar-size) disk traffic that
/// made walking a large classpath (e.g. the Hibernate suite's 241 jars, some
/// multi-MB) pathologically slow. Classpath/mounted jars are read-only for the
/// VM lifetime, so memoise the bytes (re-parsing the in-memory zip is cheap
/// relative to re-reading multi-MB files from disk hundreds of times).
///
/// Keyed on (path, [`crate::net_phase_e::archive_stamp`]) rather than the path
/// alone: an application server replaces a war/jar in place and redeploys, so a
/// path-only key serves the OLD archive forever (see `archive_stamp`'s doc for
/// the Tomcat `TestHostConfigAutomaticDeployment*` failure this caused).
pub(crate) fn jar_bytes_cached(jar: &str) -> Option<std::sync::Arc<Vec<u8>>> {
    use std::sync::{Arc, Mutex, OnceLock};
    #[allow(clippy::type_complexity)]
    static CACHE: OnceLock<
        Mutex<std::collections::HashMap<(String, u64, u64), Option<Arc<Vec<u8>>>>>,
    > = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let (mtime, len) = crate::net_phase_e::archive_stamp(jar);
    let key = (jar.to_string(), mtime, len);
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
    {
        return cached;
    }
    // A mounted nested archive uses a jarfs-encoded parent entry as its
    // container. Resolve that entry through the parent archive instead of
    // treating the sentinel representation as a host filesystem path.
    let bytes = if let Some((parent, entry)) = jarfs_decode(jar) {
        jarfs_read_entry(&parent, &entry).ok().map(Arc::new)
    } else {
        std::fs::read(jar).ok().map(Arc::new)
    };
    // Resolving a nested container recursively calls this function for its
    // parent JAR. Do that work outside the cache mutex; holding it here would
    // self-deadlock on every `jar:nested:` filesystem mount.
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|(p, _, _), _| p != jar);
    guard.insert(key, bytes.clone());
    bytes
}

/// Cached sorted entry-name index per jar (mirrors `jrt_image`). A
/// `Files.walkFileTree` over a jar calls `jarfs_classify` (per entry, via
/// `readAttributes`) and `jarfs_list_dir*` (per directory) — each of which
/// previously re-parsed the whole zip central directory, i.e. O(N²) per jar.
/// Building the sorted name list once (`ZipArchive::file_names`, no per-entry
/// decompression) turns those into O(log N) binary-search / prefix scans, which
/// is what makes walking a large classpath (the Hibernate suite's 241 jars)
/// tractable in the interpreter.
pub(crate) struct JarFsIndex {
    names: Vec<String>,
    sizes: std::collections::HashMap<String, u64>,
    raw_names: std::collections::HashMap<String, String>,
    /// Immediate children keyed by their parent directory.  Javac's archive
    /// indexer walks every package directory; deriving children by scanning a
    /// prefix range for each directory makes that first walk quadratic for
    /// deeply packaged archives.
    children: std::collections::HashMap<String, Vec<(String, bool)>>,
}

/// Keyed on (path, [`crate::net_phase_e::archive_stamp`]) — see
/// `jar_bytes_cached` for why a path-only key is wrong for a redeployable
/// archive.
pub(crate) fn jar_index(jar: &str) -> Option<std::sync::Arc<JarFsIndex>> {
    use std::sync::{Arc, Mutex, OnceLock};
    #[allow(clippy::type_complexity)]
    static CACHE: OnceLock<
        Mutex<std::collections::HashMap<(String, u64, u64), Option<Arc<JarFsIndex>>>>,
    > = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let (mtime, len) = crate::net_phase_e::archive_stamp(jar);
    let key = (jar.to_string(), mtime, len);
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
    {
        return cached;
    }
    let built = (|| {
        let bytes = jar_bytes_cached(jar)?;
        let cursor = std::io::Cursor::new(bytes.as_slice());
        let mut zip = zip::ZipArchive::new(cursor).ok()?;
        let mut names = Vec::with_capacity(zip.len());
        let mut sizes = std::collections::HashMap::with_capacity(zip.len());
        let mut raw_names = std::collections::HashMap::with_capacity(zip.len());
        let mut child_maps: std::collections::HashMap<
            String,
            std::collections::BTreeMap<String, bool>,
        > = std::collections::HashMap::new();
        for i in 0..zip.len() {
            let f = zip.by_index(i).ok()?;
            let raw = f.name().to_string();
            let name = raw.trim_start_matches('/').to_string();
            sizes.insert(name.clone(), f.size());
            raw_names.insert(name.clone(), raw);
            let trimmed = name.trim_end_matches('/');
            if !trimmed.is_empty() {
                let components: Vec<&str> =
                    trimmed.split('/').filter(|part| !part.is_empty()).collect();
                let explicit_dir = name.ends_with('/');
                let mut parent = String::new();
                for (component_index, component) in components.iter().enumerate() {
                    let child = if parent.is_empty() {
                        (*component).to_string()
                    } else {
                        format!("{parent}/{component}")
                    };
                    let is_dir = component_index + 1 < components.len() || explicit_dir;
                    let entry = child_maps
                        .entry(parent.clone())
                        .or_default()
                        .entry(child.clone())
                        .or_insert(false);
                    *entry = *entry || is_dir;
                    parent = child;
                }
            }
            names.push(name);
        }
        names.sort_unstable();
        names.dedup();
        let children = child_maps
            .into_iter()
            .map(|(parent, entries)| (parent, entries.into_iter().collect()))
            .collect();
        Some(Arc::new(JarFsIndex {
            names,
            sizes,
            raw_names,
            children,
        }))
    })();
    // Building an index for a nested archive first reads the archive entry
    // from its parent, which in turn indexes that parent.  Do not retain this
    // cache lock across that recursive work.
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|(p, _, _), _| p != jar);
    guard.insert(key, built.clone());
    built
}

pub(crate) fn jarfs_read_entry(jar: &str, entry: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let entry = entry.trim_start_matches('/');
    let jar_bytes =
        jar_bytes_cached(jar).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    let cursor = std::io::Cursor::new(jar_bytes.as_slice());
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let entry_name = jar_index(jar)
        .and_then(|idx| idx.raw_names.get(entry).cloned())
        .unwrap_or_else(|| entry.to_string());
    let mut f = zip
        .by_name(&entry_name)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    let mut buf = Vec::with_capacity(f.size().min(1 << 27) as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

pub(crate) fn jarfs_entry_size(jar: &str, entry: &str) -> std::io::Result<i64> {
    let entry = entry.trim_start_matches('/');
    let index = jar_index(jar).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    index
        .sizes
        .get(entry)
        .map(|size| *size as i64)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}

pub(crate) fn jarfs_rewrite_entry(
    jar: &str,
    entry: &str,
    data: Option<&[u8]>,
) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let entry = entry.trim_start_matches('/').trim_end_matches('/');
    if entry.is_empty() {
        if let Some(parent) = std::path::Path::new(jar).parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !std::path::Path::new(jar).exists() {
            let f = std::fs::File::create(jar)?;
            zip::ZipWriter::new(f)
                .finish()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        return Ok(());
    }
    let target = if data.is_some() {
        entry.to_string()
    } else {
        format!("{entry}/")
    };
    let mut existing: Vec<(String, Option<Vec<u8>>)> = Vec::new();
    if std::path::Path::new(jar).is_file() {
        if let Ok(file) = std::fs::File::open(jar) {
            if let Ok(mut archive) = zip::ZipArchive::new(file) {
                for i in 0..archive.len() {
                    let mut f = archive.by_index(i).map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                    })?;
                    let name = f.name().to_string();
                    if name == target || name.trim_end_matches('/') == entry {
                        continue;
                    }
                    if f.is_dir() {
                        existing.push((name, None));
                    } else {
                        let mut bytes = Vec::with_capacity(f.size().min(1 << 27) as usize);
                        f.read_to_end(&mut bytes)?;
                        existing.push((name, Some(bytes)));
                    }
                }
            }
        }
    }
    if let Some(parent) = std::path::Path::new(jar).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = format!("{jar}.cratonvm-tmp-{}", std::process::id());
    let file = std::fs::File::create(&tmp)?;
    let mut writer = zip::ZipWriter::new(file);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in existing {
        if let Some(bytes) = bytes {
            writer
                .start_file(name, opts)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            writer.write_all(&bytes)?;
        } else {
            writer
                .add_directory(name, opts)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
    }
    if let Some(bytes) = data {
        writer
            .start_file(target, opts)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        writer.write_all(bytes)?;
    } else {
        writer
            .add_directory(target, opts)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    }
    writer
        .finish()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    std::fs::rename(tmp, jar)?;
    Ok(())
}

pub(crate) fn jarfs_create_dir_entry(jar: &str, entry: &str) -> std::io::Result<()> {
    jarfs_rewrite_entry(jar, entry, None)
}

pub(crate) fn jarfs_write_file_entry(jar: &str, entry: &str, bytes: &[u8]) -> std::io::Result<()> {
    jarfs_rewrite_entry(jar, entry, Some(bytes))
}

/// Kind of a jar-FS path: regular file, directory, or absent.
pub(crate) enum JarFsKind {
    File,
    Dir,
    Absent,
}

pub(crate) fn jarfs_classify(jar: &str, entry: &str) -> JarFsKind {
    let entry = entry.trim_start_matches('/');
    // The mounted jar's ROOT is always a directory — even when the jar file
    // itself is missing. A manifest `Class-Path:` header routinely names sibling
    // jars that aren't present (e.g. Derby's `Class-Path: derbyshared.jar …`,
    // none of which live in the Gradle cache's per-artifact hash dir); javac
    // mounts each and walks its root. Reporting the root of an absent jar as
    // `Absent` made `readAttributes` throw `NoSuchFileException`, which javac
    // surfaced as a fatal "cannot access <package>" (the missing jar otherwise
    // contributes no classes, so it should walk as an EMPTY directory and be
    // skipped — matching the net effect of HotSpot, whose zip provider throws at
    // mount time and javac then skips the entry).
    if entry.is_empty() {
        return JarFsKind::Dir;
    }
    let index = match jar_index(jar) {
        Some(n) => n,
        None => return JarFsKind::Absent,
    };
    let names = &index.names;
    // Exact entry → a regular file.
    if names.binary_search_by(|n| n.as_str().cmp(entry)).is_ok() {
        return JarFsKind::File;
    }
    // An explicit directory entry (`entry/`) or any entry living under `entry/`.
    // The sorted index makes this the first name >= `entry/`.
    let dir_prefix = format!("{entry}/");
    let idx = names.partition_point(|n| n.as_str() < dir_prefix.as_str());
    if names.get(idx).is_some_and(|n| n.starts_with(&dir_prefix)) {
        return JarFsKind::Dir;
    }
    JarFsKind::Absent
}

/// List the immediate children of a directory `dir` inside a JAR. Returns
/// full entry paths (relative to the jar root).
pub(crate) fn jarfs_list_dir(jar: &str, dir: &str) -> Vec<String> {
    jarfs_list_dir_classified(jar, dir)
        .into_iter()
        .map(|(child, _)| child)
        .collect()
}

/// List immediate children of `dir` inside a JAR together with an
/// is-directory flag from the cached direct-child index.  This keeps a full
/// archive walk linear in its entries rather than repeatedly scanning every
/// descendant below each package directory.
pub(crate) fn jarfs_list_dir_classified(jar: &str, dir: &str) -> Vec<(String, bool)> {
    let dir = dir.trim_start_matches('/').trim_end_matches('/');
    let index = match jar_index(jar) {
        Some(n) => n,
        None => return Vec::new(),
    };
    index.children.get(dir).cloned().unwrap_or_default()
}

// ===========================================================================
// jrt (runtime image) filesystem — javac and any tool that compiles in-process
// reads the platform classes from the `jrt:/` filesystem, whose entries live in
// `<java.home>/lib/modules` (the jimage). CratonVM's synthetic providers list
// only "file" and "jar", so `FileSystems.getFileSystem(jrt:/)` previously threw
// `ProviderNotFoundException` and javac reported "Unable to find package
// java.lang in platform classes". The helpers below back a synthetic jrt FS
// using the existing `cratonvm_reader::JImageReader`.
//
// jrt logical layout exposed to Java: `/modules/<module>/<pkg>/<Class>.class`
// (and the container dirs `/`, `/modules`). A jrt logical entry maps to the
// jimage resource path `/<module>/<pkg>/<Class>.class`.

/// Cached jimage reader + sorted entry-path list, keyed by `java.home`. The
/// jimage is large (~140 MB) and javac walks the platform image many times, so
/// both the reader and a sorted `/<module>/<resource>` list (for prefix scans)
/// are memoised for the VM lifetime.
pub(crate) struct JrtImage {
    reader: cratonvm_reader::JImageReader,
    entries: Vec<String>,
    package_modules: std::collections::BTreeMap<String, Vec<String>>,
    entry_sizes: std::collections::HashMap<String, u64>,
}

pub(crate) fn jrt_image(java_home: &str) -> Option<std::sync::Arc<JrtImage>> {
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, Option<Arc<JrtImage>>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = guard.get(java_home) {
        return cached.clone();
    }
    let modules_path = std::path::Path::new(java_home).join("lib").join("modules");
    let built = cratonvm_reader::JImageReader::open(&modules_path)
        .ok()
        .map(|reader| {
            let mut entries = Vec::new();
            let mut entry_sizes = std::collections::HashMap::new();
            for (p, _, size) in reader.iter_entries().unwrap_or_default() {
                if p.starts_with('/') {
                    entry_sizes.insert(p.clone(), size);
                    entries.push(p);
                }
            }
            entries.sort_unstable();
            entries.dedup();
            let mut package_modules: std::collections::BTreeMap<
                String,
                std::collections::BTreeSet<String>,
            > = std::collections::BTreeMap::new();
            for p in &entries {
                let s = p.trim_start_matches('/');
                let Some((module, resource)) = s.split_once('/') else {
                    continue;
                };
                if module.is_empty() || module == "modules" || module == "packages" {
                    continue;
                }
                let Some((package, _name)) = resource.rsplit_once('/') else {
                    continue;
                };
                if package.is_empty() {
                    continue;
                }
                package_modules
                    .entry(package.to_string())
                    .or_default()
                    .insert(module.to_string());
            }
            let package_modules = package_modules
                .into_iter()
                .map(|(package, modules)| (package, modules.into_iter().collect()))
                .collect();
            Arc::new(JrtImage {
                reader,
                entries,
                package_modules,
                entry_sizes,
            })
        });
    guard.insert(java_home.to_string(), built.clone());
    built
}

/// Map a jrt logical entry (`modules/java.base/java/lang/String.class`) to the
/// jimage resource path (`/java.base/java/lang/String.class`). Returns `None`
/// for the synthetic container dirs (`""`, `modules`) which have no jimage
/// resource of their own.
pub(crate) fn jrt_entry_to_image(entry: &str) -> Option<String> {
    let e = entry.trim_matches('/');
    let rest = e.strip_prefix("modules")?;
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        None
    } else {
        Some(format!("/{rest}"))
    }
}

pub(crate) fn jrt_img_is_file(img: &JrtImage, path: &str) -> bool {
    img.entries
        .binary_search_by(|e| e.as_str().cmp(path))
        .is_ok()
}

pub(crate) fn jrt_img_is_dir(img: &JrtImage, path: &str) -> bool {
    let prefix = format!("{path}/");
    let idx = img
        .entries
        .partition_point(|e| e.as_str() < prefix.as_str());
    img.entries.get(idx).is_some_and(|e| e.starts_with(&prefix))
}

pub(crate) fn jrt_package_link_target(img: &JrtImage, rest: &str) -> Option<String> {
    let (package, module) = rest.trim_matches('/').split_once('/')?;
    if module.contains('/') {
        return None;
    }
    img.package_modules
        .get(&package.replace('.', "/"))?
        .iter()
        .any(|candidate| candidate == module)
        .then(|| format!("modules/{module}"))
}

pub(crate) fn jrt_package_backing_entry(img: &JrtImage, rest: &str) -> Option<String> {
    let mut parts = rest.trim_matches('/').split('/');
    let package = parts.next()?;
    let module = parts.next()?;
    let target = jrt_package_link_target(img, &format!("{package}/{module}"))?;
    let suffix = parts.collect::<Vec<_>>().join("/");
    Some(if suffix.is_empty() {
        target
    } else {
        format!("{target}/{suffix}")
    })
}

pub(crate) fn jrt_package_path_kind(img: &JrtImage, rest: &str) -> JarFsKind {
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        return JarFsKind::Dir;
    }
    if img.package_modules.contains_key(&rest.replace('.', "/")) {
        JarFsKind::Dir
    } else {
        let Some(backing) = jrt_package_backing_entry(img, rest) else {
            return JarFsKind::Absent;
        };
        let image_path =
            jrt_entry_to_image(&backing).expect("backing JRT package path is a module path");
        if jrt_img_is_file(img, &image_path) {
            JarFsKind::File
        } else if jrt_img_is_dir(img, &image_path) {
            JarFsKind::Dir
        } else {
            JarFsKind::Absent
        }
    }
}

pub(crate) fn jrtfs_classify(java_home: &str, entry: &str) -> JarFsKind {
    let img = match jrt_image(java_home) {
        Some(i) => i,
        None => return JarFsKind::Absent,
    };
    let e = entry.trim_matches('/');
    // Synthetic container directories that have no backing jimage resource.
    if e.is_empty() || e == "modules" || e == "packages" {
        return JarFsKind::Dir;
    }
    if let Some(rest) = e.strip_prefix("packages/") {
        return jrt_package_path_kind(&img, rest);
    }
    match jrt_entry_to_image(e) {
        Some(img_path) => {
            if jrt_img_is_file(&img, &img_path) {
                JarFsKind::File
            } else if jrt_img_is_dir(&img, &img_path) {
                JarFsKind::Dir
            } else {
                JarFsKind::Absent
            }
        }
        None => JarFsKind::Absent,
    }
}

/// List the immediate children of a jrt logical directory, returning each as a
/// full jrt logical entry (e.g. `modules/java.base/java/lang`) with an
/// is-directory flag.
pub(crate) fn jrtfs_list_dir_classified(java_home: &str, entry: &str) -> Vec<(String, bool)> {
    let img = match jrt_image(java_home) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let e = entry.trim_matches('/');
    if e.is_empty() {
        return vec![
            ("modules".to_string(), true),
            ("packages".to_string(), true),
        ];
    }
    if e == "packages" {
        let mut seen = std::collections::BTreeMap::new();
        for package in img.package_modules.keys() {
            seen.insert(format!("packages/{}", package.replace('/', ".")), true);
        }
        return seen.into_iter().collect();
    }
    if let Some(rest) = e.strip_prefix("packages/") {
        if let Some(backing) = jrt_package_backing_entry(&img, rest) {
            return jrtfs_list_dir_classified(java_home, &backing)
                .into_iter()
                .filter_map(|(child, is_dir)| {
                    child
                        .rsplit_once('/')
                        .map(|(_, name)| (format!("packages/{rest}/{name}"), is_dir))
                })
                .collect();
        }
        let mut seen: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();
        let package = rest.replace('.', "/");
        if let Some(modules) = img.package_modules.get(&package) {
            for module in modules {
                seen.insert(format!("packages/{rest}/{module}"), true);
            }
        }
        return seen.into_iter().collect();
    }
    if e == "modules" {
        // Distinct module names = the first segment of each `/<module>/...`
        // resource entry. The jimage offset table ALSO contains its own
        // internal directory-node namespaces `/modules/...` and `/packages/...`
        // (the jrt index); those first segments ("modules", "packages") are NOT
        // real modules and must be excluded, or javac treats them as modules
        // with no module-info and fails platform setup ("Unable to find package
        // java.lang in platform classes").
        let mut seen = std::collections::BTreeSet::new();
        for p in &img.entries {
            let s = p.trim_start_matches('/');
            let module = match s.find('/') {
                Some(i) => &s[..i],
                None => s,
            };
            if module.is_empty() || module == "modules" || module == "packages" {
                continue;
            }
            seen.insert(module.to_string());
        }
        return seen
            .into_iter()
            .map(|m| (format!("modules/{m}"), true))
            .collect();
    }
    match jrt_entry_to_image(e) {
        Some(img_path) => {
            let prefix = format!("{img_path}/");
            let mut seen: std::collections::BTreeMap<String, bool> =
                std::collections::BTreeMap::new();
            let start = img
                .entries
                .partition_point(|x| x.as_str() < prefix.as_str());
            for p in &img.entries[start..] {
                let rest = match p.strip_prefix(&prefix) {
                    Some(r) => r,
                    None => break, // sorted: first non-match ends the prefix range
                };
                if rest.is_empty() {
                    continue;
                }
                let (child, is_dir) = match rest.find('/') {
                    Some(j) => (&rest[..j], true),
                    None => (rest, false),
                };
                let child_entry = format!("modules{img_path}/{child}");
                let v = seen.entry(child_entry).or_insert(false);
                *v = *v || is_dir;
            }
            seen.into_iter().collect()
        }
        None => Vec::new(),
    }
}

/// Return the binary names of every class in a jrt module package.
///
/// `JavacFileManager.list` uses this view when compiling in-process.  Building
/// it from the jimage directory index keeps javac's platform-class inventory
/// complete instead of relying on a small hand-maintained class allowlist.
pub(crate) fn jrtfs_list_class_binary_names(
    java_home: &str,
    module_name: &str,
    package_name: &str,
    recurse: bool,
) -> Vec<String> {
    let package_path = package_name.replace('.', "/");
    let root = if package_path.is_empty() {
        format!("modules/{module_name}")
    } else {
        format!("modules/{module_name}/{package_path}")
    };
    let module_prefix = format!("modules/{module_name}/");
    let mut pending = vec![root];
    let mut classes = Vec::new();

    while let Some(dir) = pending.pop() {
        for (child, is_dir) in jrtfs_list_dir_classified(java_home, &dir) {
            if is_dir {
                if recurse {
                    pending.push(child);
                }
                continue;
            }
            let Some(relative) = child.strip_prefix(&module_prefix) else {
                continue;
            };
            let Some(class_path) = relative.strip_suffix(".class") else {
                continue;
            };
            classes.push(class_path.replace('/', "."));
        }
    }

    classes.sort_unstable();
    classes
}

#[cfg(test)]
pub(crate) mod jrtfs_javac_listing_tests {
    use super::jrtfs_list_class_binary_names;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::path::{Path, PathBuf};

    fn test_java_home() -> Option<PathBuf> {
        for key in ["CRATONVM_TEST_JDK", "CRATONVM_JAVA_HOME", "JAVA_HOME"] {
            if let Some(home) = cratonvm_types::flags::runtime_var_os(key).map(PathBuf::from) {
                if home.join("lib/modules").is_file() {
                    return Some(home);
                }
            }
        }
        for home in [
            "/home/victor/jdk25",
            "/usr/lib/jvm/java-21-openjdk-amd64",
            "C:/Program Files/Java/jdk-25",
        ] {
            let path = Path::new(home);
            if path.join("lib/modules").is_file() {
                return Some(path.to_path_buf());
            }
        }
        None
    }

    #[test]
    fn javac_platform_listing_uses_complete_jrt_package_inventory() {
        let Some(java_home) = test_java_home() else {
            eprintln!("JDK modules image unavailable; skipping jrt listing test");
            return;
        };
        let java_home = java_home.to_string_lossy();
        let direct = jrtfs_list_class_binary_names(&java_home, "java.base", "java.lang", false);
        assert!(
            direct.len() > 100,
            "java.lang listing was truncated: {direct:?}"
        );
        for required in ["java.lang.Object", "java.lang.Byte", "java.lang.Integer"] {
            assert!(
                direct.iter().any(|name| name == required),
                "missing {required}"
            );
        }
        let recursive = jrtfs_list_class_binary_names(&java_home, "java.base", "java.lang", true);
        assert!(
            recursive
                .iter()
                .any(|name| name == "java.lang.annotation.Retention"),
            "recursive listing omitted nested java.lang packages"
        );
    }
}

pub(crate) fn jrtfs_entry_size(java_home: &str, entry: &str) -> std::io::Result<i64> {
    let img =
        jrt_image(java_home).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    let img_path = jrt_entry_to_image(entry)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    img.entry_sizes
        .get(&img_path)
        .map(|size| *size as i64)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}

/// Read a jrt logical entry's bytes out of the jimage.
pub(crate) fn jrtfs_read(java_home: &str, entry: &str) -> std::io::Result<Vec<u8>> {
    let img =
        jrt_image(java_home).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    let img_path = jrt_entry_to_image(entry)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    img.reader
        .find_resource(&img_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}

/// Classify any virtual-FS (jar / jrt) encoded path. Returns `None` for a plain
/// host path so callers fall back to `std::fs`/`std::path` checks.
pub(crate) fn vfs_classify(p: &str) -> Option<JarFsKind> {
    if let Some((jar, entry)) = jarfs_decode(p) {
        Some(jarfs_classify(&jar, &entry))
    } else {
        jrtfs_decode(p).map(|(jh, entry)| jrtfs_classify(&jh, &entry))
    }
}

/// Read any virtual-FS (jar / jrt) encoded entry's bytes. Returns `None` for a
/// plain host path (caller reads it via `std::fs`).
pub(crate) fn vfs_read(p: &str) -> Option<std::io::Result<Vec<u8>>> {
    if let Some((jar, entry)) = jarfs_decode(p) {
        Some(jarfs_read_entry(&jar, &entry))
    } else {
        jrtfs_decode(p).map(|(jh, entry)| jrtfs_read(&jh, &entry))
    }
}

/// Immediate children of a directory `p` (host / jar / jrt), as encoded path
/// strings ready for `p57_alloc_path`.
pub(crate) fn vfs_or_host_list(p: &str) -> Vec<String> {
    if let Some((jar, dir)) = jarfs_decode(p) {
        jarfs_list_dir(&jar, &dir)
            .into_iter()
            .map(|c| jarfs_encode(&jar, &c))
            .collect()
    } else if let Some((jh, dir)) = jrtfs_decode(p) {
        jrtfs_list_dir_classified(&jh, &dir)
            .into_iter()
            .map(|(c, _)| jrtfs_encode(&jh, &c))
            .collect()
    } else {
        match std::fs::read_dir(p) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path().to_string_lossy().replace('\\', "/"))
                .collect(),
            Err(_) => vec![],
        }
    }
}

/// Is `p` (host / jar / jrt) a directory?
pub(crate) fn vfs_or_host_is_dir(p: &str) -> bool {
    match vfs_classify(p) {
        Some(kind) => matches!(kind, JarFsKind::Dir),
        None => std::path::Path::new(p).is_dir(),
    }
}

/// "Is this a directory the walker should descend into?", honouring
/// `FileVisitOption.FOLLOW_LINKS`.
///
/// Without FOLLOW_LINKS the JDK's `FileTreeWalker` reads each entry's
/// attributes with `NOFOLLOW_LINKS`, so a symbolic link — even one pointing at
/// a directory — is a *file* to the walk and is never descended into. The
/// walkers here used to ask `Path::is_dir()`, which resolves the link, so every
/// tree walk in the VM silently followed every symlink. Two consequences, both
/// observed: a `FileSystemUtils.deleteRecursively(symlink)` recursed into and
/// emptied the link's TARGET instead of unlinking the link
/// (`ApplicationTempTests`), and a symlink cycle was bounded only by
/// `max_depth`.
pub(crate) fn vfs_or_host_is_walkable_dir(p: &str, follow_links: bool) -> bool {
    match vfs_classify(p) {
        // jar/jrt namespaces have no symbolic links of their own.
        Some(kind) => matches!(kind, JarFsKind::Dir),
        None if follow_links => std::path::Path::new(p).is_dir(),
        None => std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()),
    }
}

/// Depth-first pre-order walk: pushes `p` then every descendant (encoded path
/// strings). Bounded by `depth`/`max_depth` to guard against pathological host
/// symlink cycles (jar/jrt namespaces are acyclic).
pub(crate) fn vfs_or_host_walk(
    p: &str,
    depth: usize,
    max_depth: usize,
    follow_links: bool,
    out: &mut Vec<String>,
) {
    out.push(p.to_string());
    if depth >= max_depth || !vfs_or_host_is_walkable_dir(p, follow_links) {
        return;
    }
    for child in vfs_or_host_list(p) {
        vfs_or_host_walk(&child, depth + 1, max_depth, follow_links, out);
    }
}

/// Whether a `LinkOption[]` argument asks for `NOFOLLOW_LINKS`.
///
/// `java.nio.file.LinkOption` is a single-constant enum, so a non-empty array
/// unambiguously means NOFOLLOW_LINKS — the same shortcut
/// `p59_files_read_attributes` already takes, lifted here so the
/// `exists`/`isDirectory`/`isRegularFile` predicates stop ignoring the option
/// entirely. They all resolved links, so a dangling link "did not exist" and a
/// link to a directory "was a directory" even under NOFOLLOW_LINKS.
pub(crate) fn p57_link_options_nofollow(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> bool {
    matches!(arg, Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0)
}

/// `Files.exists(Path, LinkOption...)`. Shared by the two registrations of this
/// triple (`register_phase57_nio_file` and `register_p61_files_path`) so the
/// pair cannot drift — they already had, and the NOFOLLOW_LINKS repair landed
/// on the losing copy first.
pub(crate) fn p57_files_exists_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    let Some(Value::Object(Some(path_ref))) = args.first() else {
        return false;
    };
    let path_str = p57_read_path(ctx, *path_ref);
    match vfs_classify(&path_str) {
        Some(kind) => !matches!(kind, JarFsKind::Absent),
        // NOFOLLOW_LINKS asks about the LINK, so a dangling symbolic link
        // exists. `Path::exists()` resolves the link and answered false.
        None if p57_link_options_nofollow(ctx, args.get(1)) => {
            std::fs::symlink_metadata(&path_str).is_ok()
        }
        None => std::path::Path::new(&path_str).exists(),
    }
}

/// `Files.isDirectory(Path, LinkOption...)`. Shared for the same reason as
/// [`p57_files_exists_impl`].
pub(crate) fn p57_files_is_directory_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    let Some(Value::Object(Some(path_ref))) = args.first() else {
        return false;
    };
    let path_str = p57_read_path(ctx, *path_ref);
    match vfs_classify(&path_str) {
        Some(kind) => matches!(kind, JarFsKind::Dir),
        // Under NOFOLLOW_LINKS a link to a directory is a LINK, not a directory.
        None if p57_link_options_nofollow(ctx, args.get(1)) => {
            std::fs::symlink_metadata(&path_str).is_ok_and(|m| m.is_dir())
        }
        None => std::path::Path::new(&path_str).is_dir(),
    }
}

/// `Files.isRegularFile(Path, LinkOption...)`. Shared for the same reason as
/// [`p57_files_exists_impl`].
pub(crate) fn p57_files_is_regular_file_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    let Some(Value::Object(Some(path_ref))) = args.first() else {
        return false;
    };
    let path_str = p57_read_path(ctx, *path_ref);
    match vfs_classify(&path_str) {
        Some(kind) => matches!(kind, JarFsKind::File),
        None if p57_link_options_nofollow(ctx, args.get(1)) => {
            std::fs::symlink_metadata(&path_str).is_ok_and(|m| m.is_file())
        }
        None => std::path::Path::new(&path_str).is_file(),
    }
}

/// Whether a `FileVisitOption[]`/`Set<FileVisitOption>` argument asks for
/// `FOLLOW_LINKS`.
///
/// `java.nio.file.FileVisitOption` is a single-constant enum, so — exactly like
/// the `LinkOption`/NOFOLLOW_LINKS check in `p59_files_read_attributes` — a
/// non-empty container unambiguously means FOLLOW_LINKS and needs no
/// re-entrant `toString()` probe. An absent or empty argument means "do not
/// follow", which is the JDK's default.
pub(crate) fn p57_visit_options_follow_links(
    ctx: &mut dyn NativeContext,
    arg: Option<&Value>,
) -> bool {
    let Some(Value::Object(Some(o))) = arg else {
        return false;
    };
    let o = *o;
    // Array form (`Files.walk(path, opts...)` varargs). `array_length` answers
    // 0 for a non-array, so a positive length is proof of a non-empty array and
    // nothing else needs asking.
    if ctx.array_length(o) > 0 {
        return true;
    }
    // An EMPTY array is javac's zero-arg varargs form: no options were passed,
    // so the answer is `false` and there is nothing to ask. Settle it here
    // rather than falling through to the `isEmpty` probe below — a reference
    // array reports its COMPONENT class by name (`registry.rs:2369-2376`), so
    // that probe resolved `java/nio/file/FileVisitOption.isEmpty()Z` against
    // the enum and logged a spurious `NoSuchMethodError` WARN on every
    // `Files.walk(p)` / `deleteTree`. The verdict was already correct (the
    // `_ => false` arm); only the noise was new.
    if ctx.object_is_array(o) {
        return false;
    }
    // Either an EMPTY array or the Set form
    // (`Files.walkFileTree(path, Set<FileVisitOption>, ...)`). Asking a Set is
    // the only way to tell them apart, and `isEmpty` on an array simply fails
    // to resolve — landing on the same `false` an empty array deserves.
    //
    // Do NOT discriminate on the class name first: an array's class name is not
    // reliably resolvable here, and a miss silently sent every varargs
    // `FOLLOW_LINKS` down the Set branch, which is how `Files.walk(p,
    // FOLLOW_LINKS)` kept behaving as if the option had not been passed at all.
    match ctx.invoke_virtual(o, "isEmpty", "()Z", &[]) {
        Ok(Some(Value::Int(v))) => v == 0,
        _ => false,
    }
}

/// Best-effort extraction of a `java/net/URI`'s full text. Real-JDK URIs
/// (e.g. from `URI.create`) store it in the `string` field; our synthetic
/// URIs (url_parse / Path.toUri layouts) store the full text or fragments
/// at low slots. Prefer a candidate that carries a scheme; otherwise take
/// the longest string found.
pub(crate) fn p57_uri_full_text(ctx: &mut dyn NativeContext, uri: ObjectRef) -> String {
    let mut cands: Vec<String> = Vec::new();
    if let Value::Object(Some(s)) = ctx.get_field_by_name(uri, "string") {
        if let Some(t) = ctx.read_string(s) {
            cands.push(t);
        }
    }
    // Slot 6 is the "raw" full-URI-text field in the 7-field synthetic
    // layout `URL.toURI()` allocates (scheme=0, host=1, port=2, path=3,
    // query=4, fragment=5, raw=6 — see net_phase_e.rs). That native only
    // populates path (slot 3) for `file:` scheme URLs, leaving non-file
    // schemes like `jar:` with nothing readable in slots 0..=5 besides the
    // bare scheme string at slot 0 — callers here previously fell back to
    // that bare "jar"/"jrt"/etc. text as if it were a full URI, producing a
    // garbage single-segment path. Scanning slot 6 too lets the jar:/file:
    // prefix match below find the real `jar:file:/...!/entry` text.
    for slot in 0..=6usize {
        if let Value::Object(Some(s)) = ctx.get_field(uri, slot) {
            if let Some(t) = ctx.read_string(s) {
                cands.push(t);
            }
        }
    }
    cands
        .iter()
        .find(|c| c.starts_with("jar:") || c.starts_with("file:") || c.contains("://"))
        .cloned()
        .or_else(|| cands.into_iter().max_by_key(|c| c.len()))
        .unwrap_or_default()
}

/// Returns true if `text` is a `file:`-scheme URI in *opaque* form — its
/// scheme-specific part does not begin with '/', e.g. `file:.` or `file:foo`.
/// The real JDK's `Paths.get(URI)` / `Path.of(URI)` route file-scheme URIs
/// through `Windows/UnixUriSupport.fromUri`, which rejects opaque URIs with
/// `IllegalArgumentException("URI is not hierarchical")`. Spring's
/// `PathEditor`/`FileEditor` rely on that throw to fall back to the resource
/// mechanism (e.g. `setAsText("file:.")`).
pub(crate) fn p57_uri_is_opaque_file(text: &str) -> bool {
    text.strip_prefix("file:")
        .is_some_and(|rest| !rest.starts_with('/'))
}

/// Convert `jar:file:/C:/x.jar!/entry` / `file:///C:/x.jar` URI text to the
/// OS path of the backing JAR file. Returns `None` for text that does not
/// look like a file-backed URI (empty / unparseable).
pub(crate) fn p57_jar_uri_to_os_path(text: &str) -> Option<String> {
    let t = text.split("!/").next().unwrap_or(text);
    let t = t.strip_prefix("jar:").unwrap_or(t);
    let t = t.strip_prefix("nested:").unwrap_or(t);
    let t = t
        .strip_prefix("file://")
        .or_else(|| t.strip_prefix("file:"))
        .unwrap_or(t);
    // `/C:/...` URI-path form → `C:/...`
    let t = if t.len() >= 3 && t.starts_with('/') && t.as_bytes()[2] == b':' {
        &t[1..]
    } else {
        t
    };
    let t = t.trim_end_matches('/');
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Split a file-backed `jar:` URI into its backing archive and its path within
/// that archive. This is deliberately separate from `p57_jar_uri_to_os_path`:
/// callers such as `FileSystemProvider.newFileSystem` need only the container,
/// whereas `Path.of(URI)` must retain the entry portion for `Files.*` calls.
pub(crate) fn p57_jar_uri_to_entry_path(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("jar:")?;
    // Spring Boot's `jar:nested:` URI grammar places the level separator
    // *before* the `!` (`outer.jar/!nested.jar!/entry`), whereas the regular
    // JAR grammar uses `!/' (`outer.jar!/entry`). Normalize the nested form so
    // the container walk below creates one virtual jar-FS layer per archive
    // level instead of treating `outer.jar/!nested.jar` as a host file name.
    let (rest, nested) = match rest.strip_prefix("nested:") {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let normalized;
    let rest = if nested {
        normalized = rest.replace("/!", "!/");
        normalized.as_str()
    } else {
        rest
    };
    let mut components = rest.split("!/");
    let outer = components.next()?;
    let mut container = p57_jar_uri_to_os_path(outer)?;
    // `jar:nested:/outer.jar/` is the root URI used by the split-and-restore
    // flow. It mounts the outer archive itself, so represent it with an empty
    // entry rather than rejecting it as though every jar URI named a child.
    let mut entry = components.next().unwrap_or("");
    for component in components {
        container = jarfs_encode(&container, entry);
        entry = component;
    }
    Some((container, entry.to_string()))
}

/// Build a `java.io.IOException` runtime error from a Rust IO error — used so
/// callers like SmallRye that `catch (IOException)` can recover, instead of an
/// uncatchable `IllegalStateException`.
pub(crate) fn p57_io_error(e: &std::io::Error) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IOException {
        message: e.to_string(),
    }
    .into()
}

/// Default temp directory per the JDK contract: honor the `java.io.tmpdir`
/// system property (seeded from the platform default at VM init, overridable
/// with `-Djava.io.tmpdir=...`), falling back to the platform default when the
/// property is absent or empty. Temp-file natives must NOT read
/// `std::env::temp_dir()` directly — that ignores `-Djava.io.tmpdir`, so
/// harnesses could not redirect temp files off a full `/tmp` (mockk
/// `BootJarLoader` silently downgraded to its agent-less mode and every
/// mock.hashCode() then recursed to StackOverflowError, 2026-07-15).
pub(crate) fn jdk_temp_dir(tmpdir_prop: Option<String>) -> std::path::PathBuf {
    match tmpdir_prop {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => std::env::temp_dir(),
    }
}

/// Create a fresh, empty temp file exclusively (`O_CREAT|O_EXCL`) inside
/// `dir`, returning its full path. Failures MUST surface as `IOException` —
/// that is the `File.createTempFile`/`Files.createTempFile` contract, and
/// callers rely on the exception for fallback logic (mockk's `BootJarLoader`
/// falls back to a CWD jar when the temp dir is unusable; the old
/// `let _ = std::fs::File::create(..)` swallowed the error and the divergence
/// only surfaced much later as an unrelated-looking failure).
pub(crate) fn jdk_create_temp_file(
    dir: &std::path::Path,
    prefix: &str,
    suffix: &str,
) -> Result<String, cratonvm_types::error::MethodCallFailed> {
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let full = dir.join(format!("{prefix}{nanos}{suffix}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&full)
        {
            Ok(_) => return Ok(full.to_string_lossy().into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(p57_io_error(&e)),
        }
    }
    Err(RuntimeError::IOException {
        message: format!("Unable to create temporary file in {}", dir.display()),
    }
    .into())
}

/// Read a path (host file or jar-FS entry) into a UTF-8 string.
pub(crate) fn p57_read_to_string(p: &str) -> std::io::Result<String> {
    let bytes = match vfs_read(p) {
        Some(r) => r?,
        None => std::fs::read(p)?,
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `Path.normalize()` — collapse `.`/`..` per the JDK contract. Two rules the
/// previous naive `split('/')`+unconditional-`pop` version got wrong:
///   * a `..` must **not** pop above the root: `C:\a\..\..\b` → `C:\b` (not `b`),
///     `C:\..` → `C:\` (not empty). The drive/UNC/`\` root is preserved.
///   * a leading `..` on a **relative** path is **kept** (it can't be resolved
///     without a base): `..\..\a` → `..\..\a` (not `a`).
/// Output is `/`-canonical (the internal form; `p57_alloc_path` folds, `toString`
/// renders the host separator). Roots and name elements are parsed in the HOST
/// platform's syntax ([`p57_parse_root`]): on Unix `C:a/../b` normalizes to `b`
/// (the drive prefix is an ordinary name, not a root that a `..` cannot escape)
/// and `a\b/../c` to `c` (one name element, not two).
pub(crate) fn p57_normalize_path(path: &str) -> String {
    if let Some((tag, container, entry)) = vfs_decode(path) {
        return vfs_encode(tag, &container, &p57_normalize_path(&entry));
    }
    let (root, names) = p57_parse_root(path);
    let has_root = root.is_some();
    let mut stack: Vec<&str> = Vec::new();
    for name in &names {
        match name.as_str() {
            "." => {}
            ".." => match stack.last() {
                // A real name precedes the `..` → cancel the pair.
                Some(&top) if top != ".." => {
                    stack.pop();
                }
                // Nothing to cancel: keep `..` only for a relative path; for a
                // rooted path a `..` at the root is discarded (can't go above it).
                _ => {
                    if !has_root {
                        stack.push("..");
                    }
                }
            },
            other => stack.push(other),
        }
    }
    // Reconstruct in '/'-form. The root already carries its trailing separator
    // for absolute/UNC roots; a drive-relative root (`C:`) carries none, so the
    // first name attaches directly.
    let mut out = match root {
        Some(r) => r.replace('\\', "/"),
        None => String::new(),
    };
    for (i, name) in stack.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(name);
    }
    out
}

/// `Path.relativize(other)` — construct a relative path that, resolved against
/// `base`, yields `target`. The previous `strip_prefix` version only handled the
/// case where `target` is *under* `base`; when backtracking is needed it wrongly
/// returned `target` unchanged. Compute `..` × (base-tail) + target-tail off the
/// shared root prefix. Returns `None` (caller falls back to `target`) when the
/// two paths have different roots / absoluteness, which can't be relativized.
/// Output is `/`-canonical and relative (no root).
pub(crate) fn p57_relativize(base: &str, target: &str) -> Option<String> {
    let (rb, bn) = p57_parse_root(base);
    let (rt, tn) = p57_parse_root(target);
    // Roots must match (case-insensitively, matching WindowsPath); one absolute
    // and one relative cannot be relativized.
    let norm_root = |r: &Option<String>| r.as_ref().map(|s| s.to_ascii_lowercase());
    if norm_root(&rb) != norm_root(&rt) {
        return None;
    }
    let name_eq = |a: &str, b: &str| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let mut common = 0;
    while common < bn.len() && common < tn.len() && name_eq(&bn[common], &tn[common]) {
        common += 1;
    }
    let mut parts: Vec<&str> = Vec::new();
    for _ in common..bn.len() {
        parts.push("..");
    }
    for name in &tn[common..] {
        parts.push(name);
    }
    Some(parts.join("/"))
}

/// Resolve `other` against `base` as per `Path.resolve()` semantics:
/// - If `other` is absolute, return `other`.
/// - If `other` is empty, return `base`.
/// - Otherwise, join `base` + separator + `other`.
pub(crate) fn p57_resolve_paths(base: &str, other: &str) -> String {
    // Virtual-FS aware (jar / jrt): resolve happens within the mounted
    // archive's (or runtime image's) entry namespace, preserving the scheme.
    if let Some((tag, container, base_entry)) = vfs_decode(base) {
        let other_entry = vfs_decode(other)
            .map(|(_, _, e)| e)
            .unwrap_or_else(|| other.to_string());
        if other_entry.is_empty() {
            return base.to_string();
        }
        if other_entry.starts_with('/') {
            return vfs_encode(tag, &container, &other_entry);
        }
        let be = base_entry.trim_end_matches('/');
        let joined = if be.is_empty() {
            other_entry
        } else {
            format!("{be}/{other_entry}")
        };
        return vfs_encode(tag, &container, &joined);
    }
    if other.is_empty() {
        return base.to_string();
    }
    // Is `other` absolute? A drive prefix (`C:...`) and a leading `\` are
    // Windows syntax ONLY. On Unix both are ordinary relative filenames —
    // `a:b` is a perfectly legal Unix name — and treating them as absolute
    // made `dir.resolve("a:b")` answer `a:b` instead of `dir/a:b`, silently
    // dropping the base. Same family as `p57_parse_root`.
    let other_is_absolute = if cfg!(windows) {
        other.starts_with('/')
            || (other.len() >= 2 && other.as_bytes()[1] == b':')
            || other.starts_with('\\')
    } else {
        other.starts_with('/')
    };
    if other_is_absolute {
        return other.to_string();
    }
    if base.is_empty() {
        return other.to_string();
    }
    // Likewise the join separator: `\` in a Unix path is part of a filename,
    // never a separator, so it must not select `\` as the joiner.
    let sep = if cfg!(windows) && base.contains('\\') {
        '\\'
    } else {
        '/'
    };
    let base_trimmed = base.trim_end_matches(sep);
    format!("{base_trimmed}{sep}{other}")
}

/// Extract parent directory from a path string. Returns empty string for root-only paths.
pub(crate) fn p57_parent_of(path: &str) -> String {
    if let Some((tag, container, entry)) = vfs_decode(path) {
        let trimmed = entry.trim_end_matches('/');
        return match trimmed.rfind('/') {
            Some(i) => vfs_encode(tag, &container, &entry[..i]),
            None => vfs_encode(tag, &container, ""),
        };
    }
    if path.is_empty() {
        return String::new();
    }
    // Windows: keep `.`/`..` name elements and treat the drive/UNC/root prefix as
    // a unit (HotSpot WindowsPath.getParent is a pure last-separator split). Rust's
    // Path::parent() normalizes a trailing `.` away and over-trims — see
    // `p57_win_parent_of`. `cfg!(windows)` is a const, so the helper is still
    // compiled (referenced) on every target — no dead-code warning.
    #[cfg(windows)]
    {
        p57_win_parent_of(path)
    }
    #[cfg(not(windows))]
    {
        p57_posix_parent_of(path)
    }
}

/// Allocate a synthetic default FileSystem object.
/// FileSystem = 1-field synthetic (field 0 = separator String).
pub(crate) fn p57_alloc_default_filesystem(ctx: &mut dyn NativeContext) -> ObjectRef {
    // Field 0 = separator; field 1 (P57_FS_JAR_FIELD) = mounted-JAR path (or
    // null); field 2 (P57_FS_JRT_FIELD) = mounted runtime-image java.home (or
    // null). The jrt field exists on every FS object so the jrt-aware
    // FileSystem.getPath/getRootDirectories natives can read it unconditionally.
    let fs = alloc_concurrent_synthetic(ctx, "java/nio/file/FileSystem", 3);
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh FileSystem (native stale-local family).
    let fs_pin = ctx.pin_native_root(fs);
    let sep = if cfg!(windows) { "\\" } else { "/" };
    let s = ctx.create_string(sep);
    let fs = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs, 0, Value::Object(Some(s)));
    ctx.unpin_native_roots(fs_pin);
    fs
}

pub(crate) fn p57_alloc_jar_filesystem(ctx: &mut dyn NativeContext, jar_path: &str) -> ObjectRef {
    let fs = p57_alloc_default_filesystem(ctx);
    if jar_path.is_empty() {
        return fs;
    }
    // Pin across the create_string below: a moving young GC there would
    // relocate the fresh FileSystem before we store the mounted jar path.
    let fs_pin = ctx.pin_native_root(fs);
    let jp = ctx.create_string(jar_path);
    let fs = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs, P57_FS_JAR_FIELD, Value::Object(Some(jp)));
    ctx.unpin_native_roots(fs_pin);
    fs
}

/// Allocate a synthetic runtime-image (`jrt:`) FileSystem rooted at `java_home`.
/// `getPath`/`readAttributes`/`newDirectoryStream` route through the jrt helpers
/// when they see the P57_FS_JRT_FIELD / a `JRTFS`-encoded path.
pub(crate) fn p57_alloc_jrt_filesystem(ctx: &mut dyn NativeContext, java_home: &str) -> ObjectRef {
    let fs = alloc_concurrent_synthetic(ctx, "java/nio/file/FileSystem", 3);
    // Pin across the create_strings below — a moving young GC there would
    // relocate the fresh FileSystem (native stale-local family).
    let fs_pin = ctx.pin_native_root(fs);
    let sep = ctx.create_string("/");
    let fs = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs, 0, Value::Object(Some(sep)));
    let jh = ctx.create_string(java_home);
    let fs = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs, P57_FS_JRT_FIELD, Value::Object(Some(jh)));
    ctx.unpin_native_roots(fs_pin);
    fs
}

// ---------------------------------------------------------------------------
// File.deleteOnExit
// ---------------------------------------------------------------------------

/// Paths registered via `File.deleteOnExit()`, deleted in reverse
/// registration order when the process exits (the JDK's `DeleteOnExitHook`
/// order).
static DELETE_ON_EXIT: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> =
    std::sync::OnceLock::new();

static DELETE_ON_EXIT_HOOK_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

extern "C" fn delete_on_exit_run() {
    let Some(lock) = DELETE_ON_EXIT.get() else {
        return;
    };
    let paths = match lock.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(e) => std::mem::take(&mut *e.into_inner()),
    };
    // Reverse order so a directory registered before its contents is removed
    // last, exactly as `DeleteOnExitHook` does.
    for p in paths.iter().rev() {
        // Best effort, like the JDK: a failure to delete is not reported.
        if std::fs::remove_file(p).is_err() {
            let _ = std::fs::remove_dir(p);
        }
    }
}

pub(crate) fn delete_on_exit_register(path: String) {
    let lock = DELETE_ON_EXIT.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    {
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        if g.iter().any(|p| p == &path) {
            return;
        }
        g.push(path);
    }
    if !DELETE_ON_EXIT_HOOK_INSTALLED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        // `libc::atexit` (rather than a local `extern "C"` declaration) so the
        // workspace-wide `clashing_extern_declarations = deny` lint has a
        // single source of truth for the symbol. Available on every target
        // this VM builds for, Windows UCRT included.
        unsafe {
            libc::atexit(delete_on_exit_run);
        }
    }
}

// ---------------------------------------------------------------------------
// DosFileAttributeView setters / DosFileAttributes flag reads
// ---------------------------------------------------------------------------

/// Windows `FILE_ATTRIBUTE_*` bits used by the DOS view/attribute natives.
pub(crate) const DOS_ATTR_HIDDEN: u32 = 0x0000_0002;
pub(crate) const DOS_ATTR_SYSTEM: u32 = 0x0000_0004;
pub(crate) const DOS_ATTR_ARCHIVE: u32 = 0x0000_0020;

/// Path behind a synthetic `*FileAttributeView` (slot 0 holds the `Path`).
fn attr_view_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let v = ctx.get_field(this, 0);
    extract_path_string(ctx, Some(&v))
}

fn attr_view_flag_arg(args: &[Value]) -> bool {
    args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0
}

pub(crate) fn dos_view_set_read_only(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let on = attr_view_flag_arg(args);
    let path = attr_view_path(ctx, this);
    let meta = std::fs::metadata(&path).map_err(|e| p57_io_error(&e))?;
    let mut perms = meta.permissions();
    perms.set_readonly(on);
    std::fs::set_permissions(&path, perms).map_err(|e| p57_io_error(&e))?;
    Ok(None)
}

#[cfg(windows)]
fn dos_view_set_flag(ctx: &mut dyn NativeContext, args: &[Value], flag: u32) -> MethodCallResult {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    extern "system" {
        fn SetFileAttributesW(lp_file_name: *const u16, dw_file_attributes: u32) -> i32;
    }
    let this = obj_arg(args, 0)?;
    let on = attr_view_flag_arg(args);
    let path = attr_view_path(ctx, this);
    let meta = std::fs::metadata(&path).map_err(|e| p57_io_error(&e))?;
    let mut attrs = meta.file_attributes();
    if on {
        attrs |= flag;
    } else {
        attrs &= !flag;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(&path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if unsafe { SetFileAttributesW(wide.as_ptr(), attrs) } == 0 {
        return Err(RuntimeError::IOException {
            message: format!("SetFileAttributes failed for {path}"),
        }
        .into());
    }
    Ok(None)
}

#[cfg(not(windows))]
fn dos_view_set_flag(ctx: &mut dyn NativeContext, args: &[Value], _flag: u32) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let on = attr_view_flag_arg(args);
    let path = attr_view_path(ctx, this);
    // Validate the file the way the real view does, whichever branch we take.
    std::fs::metadata(&path).map_err(|e| p57_io_error(&e))?;
    if !on {
        // Clearing a flag that can never be set here is trivially satisfied.
        return Ok(None);
    }
    Err(RuntimeError::UnsupportedOperationException {
        message: format!(
            "DOS hidden/system/archive attributes are not settable on this platform: {path}"
        ),
    }
    .into())
}

pub(crate) fn dos_view_set_hidden(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dos_view_set_flag(ctx, args, DOS_ATTR_HIDDEN)
}

pub(crate) fn dos_view_set_system(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dos_view_set_flag(ctx, args, DOS_ATTR_SYSTEM)
}

pub(crate) fn dos_view_set_archive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    dos_view_set_flag(ctx, args, DOS_ATTR_ARCHIVE)
}

/// Backing path recorded in slot 5 of a synthetic `DosFileAttributes` (see
/// `DosFileAttributeView.readAttributes`). Empty when the object predates the
/// 6-slot layout, in which case every flag read below answers `false` — the
/// old hardcoded result.
fn dos_attrs_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    if ctx.object_num_fields(this) <= 5 {
        return String::new();
    }
    match ctx.get_field(this, 5) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

#[cfg(windows)]
fn dos_attr_flag(path: &str, flag: u32) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| m.file_attributes() & flag != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn dos_attr_flag(path: &str, flag: u32) -> bool {
    // Unix has no system/archive concept; "hidden" is the dot-file convention
    // (the same rule `java.io.File.isHidden()` uses in this VM).
    if flag == DOS_ATTR_HIDDEN {
        return std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().starts_with('.'))
            .unwrap_or(false);
    }
    false
}

/// Shared body for the three `java/nio/file/FileStore` space accessors.
/// `which` is `0` = total, `1` = unallocated (free), `2` = usable.
/// Field 0 of the synthetic store holds its mount root.
pub(crate) fn file_store_space(ctx: &mut dyn NativeContext, args: &[Value], which: u8) -> i64 {
    let path = match args.first() {
        Some(Value::Object(Some(this))) => match ctx.get_field(*this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    file_disk_space_bytes(&path).map_or(0, |(total, free, usable)| match which {
        0 => total as i64,
        1 => free as i64,
        _ => usable as i64,
    })
}

/// True when a `CopyOption[]` varargs argument contains `REPLACE_EXISTING`.
/// Same `toString()` probe the `Files.move` native uses.
pub(crate) fn copy_options_replace_existing(
    ctx: &mut dyn NativeContext,
    opts: Option<&Value>,
) -> bool {
    let arr = match opts {
        Some(Value::Object(Some(a))) => *a,
        _ => return false,
    };
    // Pin: the `toString()` probe re-enters Java and a moving young GC there
    // would relocate the option array (native stale-local family).
    let pin = ctx.pin_native_root(arr);
    let len = ctx.array_length(arr);
    let mut found = false;
    for i in 0..len {
        let arr_cur = ctx.read_native_pin(pin, arr);
        if let Value::Object(Some(opt)) = ctx.get_array_element(arr_cur, i) {
            if let Ok(Some(Value::Object(Some(s)))) =
                ctx.invoke_virtual(opt, "toString", "()Ljava/lang/String;", &[])
            {
                if ctx
                    .read_string(s)
                    .unwrap_or_default()
                    .contains("REPLACE_EXISTING")
                {
                    found = true;
                    break;
                }
            }
        }
    }
    ctx.unpin_native_roots(pin);
    found
}

/// Build a REAL `java/nio/file/FileAlreadyExistsException` (same approach as
/// the `Files.move` native): the exception is caught by real library bytecode
/// and its `getFile()` may be read by real `Throwable` formatting, so it needs
/// genuine field layout rather than a guessed synthetic one.
pub(crate) fn p57_file_already_exists(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> cratonvm_types::error::MethodCallFailed {
    if let Ok(Some(Value::Object(Some(exc)))) =
        ctx.new_object("java/nio/file/FileAlreadyExistsException")
    {
        let exc_pin = ctx.pin_native_root(exc);
        let file_str = ctx.create_string(path);
        let exc_cur = ctx.read_native_pin(exc_pin, exc);
        let _ = ctx.invoke(
            "java/nio/file/FileAlreadyExistsException",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(exc_cur)), Value::Object(Some(file_str))],
        );
        let exc_cur = ctx.read_native_pin(exc_pin, exc);
        ctx.unpin_native_roots(exc_pin);
        return MethodCallFailed::ExceptionThrown(exc_cur);
    }
    RuntimeError::IOException {
        message: format!("FileAlreadyExistsException: {path}"),
    }
    .into()
}

/// The attribute-view names this VM's default `FileSystem` actually supports,
/// per platform. Single source of truth shared by
/// `FileSystem.supportedFileAttributeViews()` and
/// `FileStore.supportsFileAttributeView(...)`, which previously disagreed —
/// the latter answered `true` for every view name ever asked about.
pub(crate) fn supported_attribute_view_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["basic", "dos", "acl", "owner", "user"]
    } else {
        &["owner", "dos", "basic", "posix", "user", "unix"]
    }
}

/// Map a `FileAttributeView` subinterface name (binary or internal form) to
/// the short view name `supportedFileAttributeViews()` reports.
pub(crate) fn attribute_view_short_name(class_name: &str) -> Option<&'static str> {
    let simple = class_name
        .rsplit(['/', '.', '$'])
        .next()
        .unwrap_or(class_name);
    // `FileOwnerAttributeView` breaks the `<Kind>FileAttributeView` pattern.
    if simple == "FileOwnerAttributeView" {
        return Some("owner");
    }
    let base = simple.strip_suffix("FileAttributeView")?;
    Some(match base {
        "Basic" => "basic",
        "Dos" => "dos",
        "Posix" => "posix",
        "Acl" => "acl",
        "Owner" => "owner",
        "User" | "UserDefined" => "user",
        "Unix" => "unix",
        _ => return None,
    })
}

/// Allocate a synthetic `java/nio/file/FileStore` for `path`. Field 0 holds
/// the store's `name()` — the drive root on Windows (`C:\`), or `/` on
/// Unix — since real JDK FileStore names are the mount point, not the
/// queried path itself.
pub(crate) fn p57_alloc_file_store(ctx: &mut dyn NativeContext, path: &str) -> ObjectRef {
    let store = alloc_concurrent_synthetic(ctx, "java/nio/file/FileStore", 1);
    let name = if cfg!(windows) {
        std::path::Path::new(path)
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_else(|| "C:\\".to_string())
    } else {
        "/".to_string()
    };
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh FileStore (native stale-local family).
    let store_pin = ctx.pin_native_root(store);
    let s = ctx.create_string(&name);
    let store = ctx.read_native_pin(store_pin, store);
    ctx.set_field(store, 0, Value::Object(Some(s)));
    ctx.unpin_native_roots(store_pin);
    store
}

/// The per-VM default-FileSystem SINGLETON. Identity matters: callers
/// compare `path.getFileSystem()` against `FileSystems.getDefault()` with
/// `==`/`Object.equals` (JUnit's @TempDir File precondition, cassandra's
/// File(Path) ctor). Allocating a fresh synthetic FileSystem per call broke
/// that identity — every File-typed @TempDir died with "Failed to create
/// default temp directory". Stash the singleton in the REAL
/// `FileSystems$DefaultFileSystemHolder.defaultFileSystem` static field:
/// static storage is a GC root (so the ref stays valid across moving
/// collections), and the holder's real `<clinit>` never runs because
/// `FileSystems.getDefault()` is intercepted. Falls back to a fresh
/// allocation only if the holder class is unavailable.
pub(crate) fn p57_default_filesystem_singleton(ctx: &mut dyn NativeContext) -> ObjectRef {
    const HOLDER: &str = "java/nio/file/FileSystems$DefaultFileSystemHolder";
    // `class_id_by_name` is lookup-only and nothing else loads the private
    // holder class (its bytecode `<clinit>` never runs because getDefault is
    // intercepted) — load it on first use so the static stash slot exists.
    if ctx.class_id_by_name(HOLDER).is_none() {
        let _ = ctx.load_class(HOLDER);
    }
    let slot = ctx.class_id_by_name(HOLDER).and_then(|cid| {
        ctx.static_field_index_by_name(cid, "defaultFileSystem")
            .map(|idx| (cid, idx))
    });
    if let Some((cid, idx)) = slot {
        if let Value::Object(Some(fs)) = ctx.get_static_field(cid, idx) {
            return fs;
        }
        let fs = p57_alloc_default_filesystem(ctx);
        ctx.set_static_field(cid, idx, Value::Object(Some(fs)));
        return fs;
    }
    p57_alloc_default_filesystem(ctx)
}

pub(crate) fn p57_alloc_enum(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    ordinal: i32,
) -> MethodCallResult {
    let obj = alloc_concurrent_synthetic(ctx, class, 2);
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh enum (native stale-local family).
    let obj_pin = ctx.pin_native_root(obj);
    let n = ctx.create_string(name);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 0, Value::Object(Some(n)));
    ctx.set_field(obj, 1, Value::Int(ordinal));
    ctx.unpin_native_roots(obj_pin);
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// java.io.RandomAccessFile — 2-field (fd_id=0 Int, mode=1 Int)
// mode: 0=read-only, 1=read-write
// ---------------------------------------------------------------------------

/// Resolve the real-JDK `fd` FileDescriptor object on a RandomAccessFile, if
/// the loaded class declares the field. Returns `None` for the synthetic
/// 2-slot layout (where slot 0 is the raw `Int` fd_id).
pub(crate) fn raf_fd_object(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "fd") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Store an open RAF fd id. Mirrors `fos_set_fd`: when the real-JDK
/// `FileDescriptor` field is present on the receiver we stash the id on the
/// `FileDescriptor` itself (so the JDK `getFD()` bytecode returns a non-null
/// FD), allocating a fresh `FileDescriptor` when the JDK ctor did not run.
/// On the synthetic 2-slot layout we keep the legacy slot-0 placement.
pub(crate) fn raf_set_fd(ctx: &mut dyn NativeContext, this: ObjectRef, fd: u32) {
    // Real-JDK path: there's an `fd` Object field — make sure it points at a
    // live `FileDescriptor` and write the id there.
    let cid = ctx.class_id_of_object(this);
    let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
    let has_fd_field = ctx.resolve_field_index(&class_name, "fd").is_some();
    if has_fd_field {
        let fd_obj = match raf_fd_object(ctx, this) {
            Some(existing) => existing,
            None => {
                // JDK ctor did not initialise `this.fd`; allocate one so the
                // `getFD()` bytecode (`return this.fd;`) returns a non-null
                // FileDescriptor. `FSDirectory.sync` does exactly this:
                // `new RandomAccessFile(...).getFD().sync()`. Without this,
                // sync() is dispatched on a null receiver and the Lucene
                // commit path fails with InvocationTargetException.
                // Pin across the alloc below — a moving young GC there would
                // relocate `this` (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let new_fd = alloc_concurrent_synthetic(ctx, "java/io/FileDescriptor", 1);
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field_by_name(this, "fd", Value::Object(Some(new_fd)));
                ctx.unpin_native_roots(this_pin);
                new_fd
            }
        };
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd as i32));
        ctx.set_field_by_name(fd_obj, "handle", Value::Long(fd as i64));
        return;
    }
    ctx.set_field(this, 0, Value::Int(fd as i32));
}

/// Recover the open RAF fd id. Tries the real-JDK `fd.fd`/`fd.handle` pair
/// first, then falls back to slot 0 for the synthetic 2-slot layout.
pub(crate) fn raf_get_fd(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u32> {
    if let Some(fd_obj) = raf_fd_object(ctx, this) {
        match ctx.get_field_by_name(fd_obj, "fd") {
            Value::Int(v) if v >= 0 => return Some(v as u32),
            _ => {}
        }
        match ctx.get_field_by_name(fd_obj, "handle") {
            Value::Long(v) if v >= 0 => return Some(v as u32),
            _ => {}
        }
    }
    match ctx.get_field(this, 0) {
        Value::Int(v) if v >= 0 => return Some(v as u32),
        _ => None,
    }
}

/// Translate a gated `RandomAccessFile` open failure.
///
/// A capability refusal becomes the `SecurityException` it is — it happened
/// before the syscall and must not be retried. An I/O failure keeps the exact
/// `Cannot open {path}: {err}` `IOException` these constructors have always
/// thrown, so nothing that catches it changes behaviour.
fn raf_open_failure(
    path: &str,
) -> impl FnOnce(cratonvm_native_api::fd_table::FdCapabilityError) -> MethodCallFailed + '_ {
    move |err| match err {
        cratonvm_native_api::fd_table::FdCapabilityError::Denied(denied) => denied.into(),
        cratonvm_native_api::fd_table::FdCapabilityError::Io(io) => RuntimeError::IOException {
            message: format!("Cannot open {}: {}", path, io),
        }
        .into(),
    }
}

pub(crate) fn register_phase57_random_access_file(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let raf = "java/io/RandomAccessFile";

    // <init>(String name, String mode)
    r.register(
        raf,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "null path".into(),
                    }
                    .into())
                }
            };
            let mode_str = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => "r".into(),
            };
            let writable = mode_str.contains('w');
            let create = writable; // create file if mode is "rw"/"rws"/"rwd"

            if !writable {
                // Read-only mode: use open_read.
                // GAP I2 — gated; see `newFileChannel`.
                let open_result =
                    match crate::capability_gate::open_read_write_gated(&*ctx, &path, false) {
                        Ok(fd) => Ok(fd),
                        // Fallback: try read-only open. A `FileWrite` refusal
                        // takes the same fallback an `EACCES` would — mode "r"
                        // only ever asked to read.
                        Err(_) => crate::capability_gate::open_read_gated(&*ctx, &path),
                    };
                let fd_id = open_result.map_err(raf_open_failure(&path))?;
                raf_set_fd(ctx, this, fd_id);
                ctx.set_field(this, 1, Value::Int(0)); // read-only
            } else {
                let fd_id =
                    crate::capability_gate::open_read_write_gated(&*ctx, &path, create)
                        .map_err(raf_open_failure(&path))?;
                raf_set_fd(ctx, this, fd_id);
                ctx.set_field(this, 1, Value::Int(1)); // read-write
            }
            Ok(None)
        },
    );

    // <init>(File file, String mode)
    r.register(
        raf,
        "<init>",
        "(Ljava/io/File;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // File.path / File.holder.path / synthetic slot-0 fallback chain.
            let path = match args.get(1) {
                Some(Value::Object(Some(f))) => {
                    // Real-JDK `java.io.File` exposes the absolute path through
                    // the private `path` field (a `String`). Try that first; fall
                    // back to the synthetic slot-0 layout for the legacy probes.
                    let by_name = ctx.get_field_by_name(*f, "path");
                    let p = match by_name {
                        Value::Object(Some(s)) => ctx.read_string(s),
                        _ => match ctx.get_field(*f, 0) {
                            Value::Object(Some(s)) => ctx.read_string(s),
                            _ => None,
                        },
                    };
                    p.unwrap_or_default()
                }
                _ => String::new(),
            };
            if crate::nbflags().dbg_raf_init {
                eprintln!("[RAF_INIT] file ctor path='{}'", path);
            }
            let mode_str = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => "r".into(),
            };
            let writable = mode_str.contains('w');
            // GAP I2 — gated; see `newFileChannel`.
            let fd_id = crate::capability_gate::open_read_write_gated(&*ctx, &path, writable)
                .map_err(raf_open_failure(&path))?;
            raf_set_fd(ctx, this, fd_id);
            ctx.set_field(this, 1, Value::Int(if writable { 1 } else { 0 }));
            Ok(None)
        },
    );

    // read() -> int (single byte, -1 on EOF)
    r.register(raf, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        if fd_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let mut buf = [0u8; 1];
        match ctx.fd_table().rw_read(fd_id as u32, &mut buf) {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(_) => Ok(Some(Value::Int(buf[0] as i32))),
            Err(_) => Ok(Some(Value::Int(-1))),
        }
    });

    // read(byte[], int off, int len) -> int
    r.register(raf, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        if fd_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        // BUG nb-phases-late(1): read off/len as signed i32 and bounds-check
        // BEFORE casting to usize. Previously `int len` was taken `as usize`,
        // so a negative len sign-extended to ~1.8e19 and `vec![0u8; len]`
        // aborted the process. The JDK validates and throws
        // IndexOutOfBoundsException instead (ArrayIndexOutOfBounds is a subclass).
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let arr_len = ctx.array_length(arr) as i64;
        if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(if off < 0 {
                    off
                } else {
                    off.wrapping_add(len)
                })
                .into(),
            );
        }
        let off = off as usize;
        let len = len as usize;
        if len == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut buf = vec![0u8; len];
        match ctx.fd_table().rw_read(fd_id as u32, &mut buf) {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                for i in 0..n {
                    ctx.set_array_element(arr, off + i, Value::Int(buf[i] as i8 as i32));
                }
                Ok(Some(Value::Int(n as i32)))
            }
            Err(_) => Ok(Some(Value::Int(-1))),
        }
    });

    // read(byte[]) -> int
    r.register(raf, "read", "([B)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        if fd_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let len = ctx.array_length(arr);
        let mut buf = vec![0u8; len];
        match ctx.fd_table().rw_read(fd_id as u32, &mut buf) {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                for i in 0..n {
                    ctx.set_array_element(arr, i, Value::Int(buf[i] as i8 as i32));
                }
                Ok(Some(Value::Int(n as i32)))
            }
            Err(_) => Ok(Some(Value::Int(-1))),
        }
    });

    // readFully(byte[])
    r.register(raf, "readFully", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                return Err(RuntimeError::IOException {
                    message: "null buffer".into(),
                }
                .into())
            }
        };
        let len = ctx.array_length(arr);
        let mut buf = vec![0u8; len];
        let mut total = 0;
        while total < len {
            match ctx.fd_table().rw_read(fd_id as u32, &mut buf[total..]) {
                Ok(0) => {
                    return Err(RuntimeError::IOException {
                        message: "Unexpected end of file".into(),
                    }
                    .into())
                }
                Ok(n) => total += n,
                Err(e) => {
                    return Err(RuntimeError::IOException {
                        message: e.to_string(),
                    }
                    .into())
                }
            }
        }
        for i in 0..len {
            ctx.set_array_element(arr, i, Value::Int(buf[i] as i8 as i32));
        }
        Ok(None)
    });

    // readFully(byte[], int off, int len)
    r.register(raf, "readFully", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                return Err(RuntimeError::IOException {
                    message: "null buffer".into(),
                }
                .into())
            }
        };
        // BUG nb-phases-late(1): same bounds-check as read([BII) — validate
        // signed off/len before casting to usize to avoid a negative-len
        // sign-extension alloc abort. JDK throws IndexOutOfBoundsException.
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let arr_len = ctx.array_length(arr) as i64;
        if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(if off < 0 {
                    off
                } else {
                    off.wrapping_add(len)
                })
                .into(),
            );
        }
        let off = off as usize;
        let len = len as usize;
        let mut buf = vec![0u8; len];
        let mut total = 0;
        while total < len {
            match ctx.fd_table().rw_read(fd_id as u32, &mut buf[total..]) {
                Ok(0) => {
                    return Err(RuntimeError::IOException {
                        message: "Unexpected end of file".into(),
                    }
                    .into())
                }
                Ok(n) => total += n,
                Err(e) => {
                    return Err(RuntimeError::IOException {
                        message: e.to_string(),
                    }
                    .into())
                }
            }
        }
        for i in 0..len {
            ctx.set_array_element(arr, off + i, Value::Int(buf[i] as i8 as i32));
        }
        Ok(None)
    });

    // write(int)
    r.register(raf, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        ctx.fd_table()
            .rw_write(fd_id as u32, &[b])
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    });

    // write(byte[], int off, int len)
    r.register(raf, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(None),
        };
        // BUG nb-phases-late(1): validate signed off/len before the loop so a
        // negative len cannot sign-extend into a huge `Vec::with_capacity`.
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let arr_len = ctx.array_length(arr) as i64;
        if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(if off < 0 {
                    off
                } else {
                    off.wrapping_add(len)
                })
                .into(),
            );
        }
        let off = off as usize;
        let len = len as usize;
        let mut buf = Vec::with_capacity(len);
        for i in 0..len {
            if let Value::Int(b) = ctx.get_array_element(arr, off + i) {
                buf.push(b as u8);
            }
        }
        ctx.fd_table()
            .rw_write(fd_id as u32, &buf)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    });

    // write(byte[])
    r.register(raf, "write", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(None),
        };
        let len = ctx.array_length(arr);
        let mut buf = Vec::with_capacity(len);
        for i in 0..len {
            if let Value::Int(b) = ctx.get_array_element(arr, i) {
                buf.push(b as u8);
            }
        }
        ctx.fd_table()
            .rw_write(fd_id as u32, &buf)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    });

    // seek(long pos)
    r.register(raf, "seek", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        // RKC23B: J-typed args may arrive tagged as Double across some
        // native dispatch paths; reinterpret bits to recover the long.
        let pos = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
            _ => 0,
        };
        ctx.fd_table()
            .rw_seek(fd_id as u32, std::io::SeekFrom::Start(pos.max(0) as u64))
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    });

    // getFilePointer() -> long
    r.register(raf, "getFilePointer", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let pos = ctx.fd_table().rw_position(fd_id as u32).unwrap_or(0);
        Ok(Some(Value::Long(pos as i64)))
    });

    // length() -> long
    r.register(raf, "length", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let size = ctx.fd_table().file_size(fd_id as u32).unwrap_or(0);
        Ok(Some(Value::Long(size as i64)))
    });

    // setLength(long newLength)
    r.register(raf, "setLength", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let new_len = match args.get(1) {
            Some(Value::Long(v)) => *v as u64,
            Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()) as u64,
            _ => 0,
        };
        ctx.fd_table()
            .rw_set_length(fd_id as u32, new_len)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    });

    // close()
    r.register(raf, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        if fd_id >= 0 {
            let _ = ctx.fd_table().close(fd_id as u32);
            // Clear via the same path `<init>` used so a real-JDK receiver's
            // `FileDescriptor` is updated rather than its `fd` Object slot
            // being stomped with an `Int`.
            if let Some(fd_obj) = raf_fd_object(ctx, this) {
                ctx.set_field_by_name(fd_obj, "fd", Value::Int(-1));
                ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
            } else {
                ctx.set_field(this, 0, Value::Int(-1));
            }
        }
        Ok(None)
    });

    // --- DataInput interface methods ---
    r.register(raf, "readInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 4];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(i32::from_be_bytes(buf))))
    });
    r.register(raf, "readLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 8];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Long(i64::from_be_bytes(buf))))
    });
    r.register(raf, "readShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 2];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(i16::from_be_bytes(buf) as i32)))
    });
    r.register(raf, "readChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 2];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(u16::from_be_bytes(buf) as i32)))
    });
    r.register(raf, "readByte", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 1];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(buf[0] as i8 as i32)))
    });
    r.register(raf, "readUnsignedByte", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 1];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(buf[0] as i32)))
    });
    r.register(raf, "readUnsignedShort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 2];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(u16::from_be_bytes(buf) as i32)))
    });
    r.register(raf, "readBoolean", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 1];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(if buf[0] != 0 { 1 } else { 0 })))
    });
    r.register(raf, "readFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 4];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Float(f32::from_be_bytes(buf))))
    });
    r.register(raf, "readDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 8];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Double(f64::from_be_bytes(buf))))
    });
    r.register(raf, "readUTF", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut len_buf = [0u8; 2];
        raf_read_fully(ctx, fd_id, &mut len_buf)?;
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut str_buf = vec![0u8; len];
        raf_read_fully(ctx, fd_id, &mut str_buf)?;
        let s = String::from_utf8_lossy(&str_buf).to_string();
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(raf, "readLine", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            match ctx.fd_table().rw_read(fd_id as u32, &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if buf[0] == b'\n' {
                        break;
                    }
                    if buf[0] != b'\r' {
                        line.push(buf[0]);
                    }
                }
                Err(_) => break,
            }
        }
        if line.is_empty()
            && ctx.fd_table().rw_position(fd_id as u32).unwrap_or(0)
                >= ctx.fd_table().file_size(fd_id as u32).unwrap_or(0)
        {
            return Ok(Some(Value::Object(None))); // EOF
        }
        let s = String::from_utf8_lossy(&line).to_string();
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });

    // --- DataOutput interface methods ---
    r.register(raf, "writeInt", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let _ = ctx.fd_table().rw_write(fd_id as u32, &v.to_be_bytes());
        Ok(None)
    });
    r.register(raf, "writeLong", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = match args.get(1) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        let _ = ctx.fd_table().rw_write(fd_id as u32, &v.to_be_bytes());
        Ok(None)
    });
    r.register(raf, "writeShort", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        let _ = ctx.fd_table().rw_write(fd_id as u32, &v.to_be_bytes());
        Ok(None)
    });
    r.register(raf, "writeChar", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u16;
        let _ = ctx.fd_table().rw_write(fd_id as u32, &v.to_be_bytes());
        Ok(None)
    });
    r.register(raf, "writeByte", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        let _ = ctx.fd_table().rw_write(fd_id as u32, &[v]);
        Ok(None)
    });
    r.register(raf, "writeBoolean", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let _ = ctx
            .fd_table()
            .rw_write(fd_id as u32, &[if v != 0 { 1 } else { 0 }]);
        Ok(None)
    });
    r.register(raf, "writeFloat", "(F)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = match args.get(1) {
            Some(Value::Float(f)) => *f,
            _ => 0.0,
        };
        let _ = ctx.fd_table().rw_write(fd_id as u32, &v.to_be_bytes());
        Ok(None)
    });
    r.register(raf, "writeDouble", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = match args.get(1) {
            Some(Value::Double(d)) => *d,
            _ => 0.0,
        };
        let _ = ctx.fd_table().rw_write(fd_id as u32, &v.to_be_bytes());
        Ok(None)
    });
    r.register(raf, "writeUTF", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        let bytes = s.as_bytes();
        let len = bytes.len().min(65535) as u16;
        let _ = ctx.fd_table().rw_write(fd_id as u32, &len.to_be_bytes());
        let _ = ctx
            .fd_table()
            .rw_write(fd_id as u32, &bytes[..len as usize]);
        Ok(None)
    });
    r.register(raf, "writeBytes", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        let _ = ctx.fd_table().rw_write(fd_id as u32, s.as_bytes());
        Ok(None)
    });
    r.register(raf, "writeChars", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        for ch in s.chars() {
            let _ = ctx
                .fd_table()
                .rw_write(fd_id as u32, &(ch as u16).to_be_bytes());
        }
        Ok(None)
    });
    r.register(raf, "getFD", "()Ljava/io/FileDescriptor;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Prefer the receiver's real-JDK `fd` field when it exists (and is
        // non-null) — Lucene's `FSDirectory.sync` immediately calls
        // `getFD().sync()`, so a freshly-allocated wrapper that does NOT
        // carry the open fd into a `FileDescriptor.sync()` capable shape
        // would NPE the bytecode caller. The `<init>` natives now ensure
        // `this.fd` is populated for real-JDK receivers.
        if let Some(existing) = raf_fd_object(ctx, this) {
            if crate::nbflags().dbg_raf_getfd {
                eprintln!("[RAF_GETFD] existing fd_obj returned");
            }
            return Ok(Some(Value::Object(Some(existing))));
        }
        // Synthetic 2-slot layout fallback: allocate a FileDescriptor that
        // carries the fd id in *both* the real-JDK-shaped name fields (`fd`
        // / `handle`) and the legacy slot 0. Real-JDK `FileDescriptor.sync`
        // bytecode reads `this.handle == -1L && this.fd == -1` before
        // dispatching to `sync0()`; if those fields are missing the read
        // is treated as out-of-bounds, falls back to null/0, and the sync
        // bytecode still proceeds (sync0 is a no-op). The synthetic must
        // also remain non-null on return — `Ok(Some(Value::Object(Some(fd))))`
        // — or `file.getFD().sync()` NPEs at the next bytecode step.
        let fd_id = ctx.get_field(this, 0);
        let fd = alloc_concurrent_synthetic(ctx, "java/io/FileDescriptor", 1);
        ctx.set_field(fd, 0, fd_id);
        // Mirror the id into the named real-JDK slots when present so that
        // `FileDescriptor.valid()` / `.sync()` see a non-(-1) value.
        let cid = ctx.class_id_of_object(fd);
        let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
        if ctx.resolve_field_index(&class_name, "fd").is_some() {
            ctx.set_field_by_name(fd, "fd", fd_id);
        }
        if ctx.resolve_field_index(&class_name, "handle").is_some() {
            let id_long = match fd_id {
                Value::Int(v) => Value::Long(v as i64),
                v => v,
            };
            ctx.set_field_by_name(fd, "handle", id_long);
        }
        if crate::nbflags().dbg_raf_getfd {
            eprintln!("[RAF_GETFD] synthetic fd_obj allocated, fd_id={:?}", fd_id);
        }
        Ok(Some(Value::Object(Some(fd))))
    });
    r.register(
        raf,
        "getChannel",
        "()Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fc = alloc_concurrent_synthetic(ctx, "java/nio/channels/FileChannel", 1);
            ctx.set_field(fc, 0, ctx.get_field(this, 0));
            Ok(Some(Value::Object(Some(fc))))
        },
    );
    r.register(raf, "skipBytes", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let n = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if n <= 0 {
            return Ok(Some(Value::Int(0)));
        }
        let pos = ctx.fd_table().rw_position(fd_id as u32).unwrap_or(0);
        let size = ctx.fd_table().file_size(fd_id as u32).unwrap_or(0);
        let skip = (n as u64).min(size.saturating_sub(pos));
        let _ = ctx
            .fd_table()
            .rw_seek(fd_id as u32, std::io::SeekFrom::Current(skip as i64));
        Ok(Some(Value::Int(skip as i32)))
    });
    r.set_category(__prev_cat);
}

/// Read exactly `buf.len()` bytes from a RAF fd, or error.
pub(crate) fn raf_read_fully(
    ctx: &mut dyn NativeContext,
    fd_id: i32,
    buf: &mut [u8],
) -> Result<(), MethodCallFailed> {
    let mut total = 0;
    while total < buf.len() {
        match ctx.fd_table().rw_read(fd_id as u32, &mut buf[total..]) {
            Ok(0) => {
                return Err(RuntimeError::IOException {
                    message: "Unexpected end of file".into(),
                }
                .into())
            }
            Ok(n) => total += n,
            Err(e) => {
                return Err(RuntimeError::IOException {
                    message: e.to_string(),
                }
                .into())
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// java.io.File — 1-field synthetic (path=0 String)
// Real filesystem operations via std::fs and std::path
// ---------------------------------------------------------------------------

/// Read the path string from a File object (field 0).
/// Percent-encode a slashified absolute path for use in a `file:` URI,
/// matching `sun.net.www.ParseUtil.encodePath` / what `File.toURI()` produces.
/// Leaves the path separator `/` and the RFC 2396 path-segment characters
/// unreserved; everything else (including space, `^`, `#`, `?`, `%`,
/// non-ASCII bytes via UTF-8) is `%XX`-encoded.
pub(crate) fn encode_file_uri_path(path: &str) -> String {
    // Unreserved (RFC 2396 §2.3) + the sub-delims/path chars the JDK leaves
    // literal in a file URI path: letters, digits, `_-.!~*'()`, plus the
    // path-meaningful `/`, `:`, `@`, `&`, `=`, `+`, `$`, `,`. The JDK's
    // ParseUtil keeps `:` (drive letter / scheme-ish) and these sub-delims.
    fn keep(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'_'
                    | b'-'
                    | b'.'
                    | b'!'
                    | b'~'
                    | b'*'
                    | b'\''
                    | b'('
                    | b')'
                    | b':'
                    | b'@'
                    | b'&'
                    | b'='
                    | b'+'
                    | b'$'
                    | b','
            )
    }
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if keep(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

/// Set a path's last-modified time, in milliseconds since the epoch, the way
/// `java.io.File.setLastModified(long)` must: it has to work for
/// **directories** as well as regular files, and report failure (`false`) when
/// the path does not exist.
///
/// The obvious `OpenOptions::new().write(true).open(path)` + `File::set_modified`
/// spelling silently gets this wrong for directories on every platform — on
/// Windows `CreateFileW` refuses a directory handle without
/// `FILE_FLAG_BACKUP_SEMANTICS`, and on Unix `open(2)` with `O_WRONLY` returns
/// `EISDIR` — so it returned `false` for every directory. Tomcat's
/// `TestHostConfigAutomaticDeploymentUpdateWarOffline` calls
/// `dir.setLastModified(...)` on the expanded webapp directory to age it and
/// asserts the return value, so it failed all four of its tests on CratonVM
/// while passing on HotSpot. `filetime::set_file_mtime` opens with the right
/// flags on both platforms.
pub(crate) fn set_file_mtime_millis(path: &str, millis: i64) -> bool {
    // Split into whole seconds + non-negative nanosecond remainder, which is
    // what `FileTime::from_unix_time` expects (its nanos argument must be in
    // `0..1_000_000_000` even for pre-epoch timestamps).
    let secs = millis.div_euclid(1000);
    let nanos = (millis.rem_euclid(1000) * 1_000_000) as u32;
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(secs, nanos)).is_ok()
}

// ---------------------------------------------------------------------------
// `java.io.FileSystem` native surface (UnixFileSystem / WinNTFileSystem)
// ---------------------------------------------------------------------------
//
// JDK 22 moved almost every `FileSystem` JNI native behind a `0`-suffixed
// private one with a plain-Java public wrapper in front of it:
// `getBooleanAttributes0`, `checkAccess0`, `getLength0`, `getLastModifiedTime0`,
// `list0`, `createDirectory0`, `setLastModifiedTime0`, `setReadOnly0`,
// `setPermission0`, `createFileExclusively0`, `getSpace0`, `getNameMax0`.
//
// That matters here because the wrappers are *concrete bytecode* and neither
// `java/io/UnixFileSystem` nor `java/io/WinNTFileSystem` appears in the
// `check_override` allow-list in `vm_exec.rs::invoke_on_class_shared_inner`
// — so a registered native under the bare pre-22 name is inert on a real
// JDK 22+: the wrapper's bytecode runs and asks for the `0` native, which was
// never registered. `File.exists()` then dies with
//
//     UnsatisfiedLinkError: java/io/UnixFileSystem.getBooleanAttributes0(Ljava/io/File;)I
//
// (fixed-suite-bugs/springboot/
// unixfilesystem-getbooleanattributes0-missing-native-20260804-FIXED.md).
// The `java/io/File` natives normally hide this, because `File.exists` /
// `isFile` / `isDirectory` ARE forced overrides — but only until some test
// redefines `java.io.File` (Mockito's inline mock maker does that
// process-wide for `@Mock private File f`), after which the real `File`
// bytecode runs for every instance and every one of these natives is reached.
//
// Both spellings are registered: the `0` one for JDK 22+, the bare one for
// pre-22 JDKs and for CratonVM's synthetic `java.io.FileSystem`.

/// `java.io.FileSystem.BA_EXISTS`.
pub(crate) const FS_BA_EXISTS: i32 = 0x01;
/// `java.io.FileSystem.BA_REGULAR`.
pub(crate) const FS_BA_REGULAR: i32 = 0x02;
/// `java.io.FileSystem.BA_DIRECTORY`.
pub(crate) const FS_BA_DIRECTORY: i32 = 0x04;
/// `java.io.FileSystem.BA_HIDDEN`.
pub(crate) const FS_BA_HIDDEN: i32 = 0x08;

/// `java.io.FileSystem.ACCESS_EXECUTE`.
pub(crate) const FS_ACCESS_EXECUTE: i32 = 0x01;
/// `java.io.FileSystem.ACCESS_WRITE`.
pub(crate) const FS_ACCESS_WRITE: i32 = 0x02;
/// `java.io.FileSystem.ACCESS_READ`.
pub(crate) const FS_ACCESS_READ: i32 = 0x04;

/// The bitmask the raw `getBooleanAttributes0` JNI native returns.
///
/// Note the platform split on `BA_HIDDEN`, which is NOT cosmetic:
/// `UnixFileSystem.getBooleanAttributes0` never sets it (the public wrapper
/// ORs in `isHidden(f)`, a leading-`.` check on the *name*), while
/// `WinNTFileSystem.getBooleanAttributes0` does, straight out of
/// `FILE_ATTRIBUTE_HIDDEN`, and its wrapper adds nothing. Getting this
/// backwards makes `File.isHidden()` wrong on one platform or the other.
pub(crate) fn fs_boolean_attributes0(path: &str) -> i32 {
    let md = match std::fs::metadata(path) {
        Ok(m) => m,
        // Real JNI returns 0 for anything it cannot `stat`, including a
        // permission error on a parent directory — not just ENOENT.
        Err(_) => return 0,
    };
    let mut attrs = FS_BA_EXISTS;
    if md.is_file() {
        attrs |= FS_BA_REGULAR;
    }
    if md.is_dir() {
        attrs |= FS_BA_DIRECTORY;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
        if md.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0 {
            attrs |= FS_BA_HIDDEN;
        }
    }
    attrs
}

/// The public `getBooleanAttributes` wrapper's result.
///
/// On Unix that is `getBooleanAttributes0(f) | isHidden(f)`, and `isHidden`
/// is a pure name test that does not consult the filesystem — so a
/// non-existent `.foo` reports `BA_HIDDEN` and nothing else, exactly as
/// HotSpot does. On Windows the wrapper returns `getBooleanAttributes0`
/// unchanged.
pub(crate) fn fs_boolean_attributes(path: &str) -> i32 {
    let attrs = fs_boolean_attributes0(path);
    #[cfg(not(windows))]
    {
        let dot_hidden = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().starts_with('.'))
            .unwrap_or(false);
        if dot_hidden {
            return attrs | FS_BA_HIDDEN;
        }
    }
    attrs
}

/// `checkAccess0(File, int)` — one of `ACCESS_READ`/`WRITE`/`EXECUTE`.
///
/// This used to answer plain "does the path exist?" for every mode, so
/// `File.canWrite()` on a file that `File.setReadOnly()` had just made
/// read-only still said `true` whenever the real `FileSystem` bytecode was
/// the caller.
pub(crate) fn fs_check_access(path: &str, access: i32) -> bool {
    #[cfg(unix)]
    {
        // `access(2)`, the same call the JDK's native makes — real uid/gid,
        // not effective, and it honours ACLs and read-only mounts that a
        // permission-bit test alone would miss.
        let mode = match access {
            FS_ACCESS_EXECUTE => libc::X_OK,
            FS_ACCESS_WRITE => libc::W_OK,
            FS_ACCESS_READ => libc::R_OK,
            _ => libc::F_OK,
        };
        let c_path = match std::ffi::CString::new(path) {
            Ok(c) => c,
            Err(_) => return false,
        };
        // SAFETY: `c_path` is a NUL-terminated C string that outlives the call,
        // and `access(2)` only reads it.
        unsafe { libc::access(c_path.as_ptr(), mode) == 0 }
    }
    #[cfg(not(unix))]
    {
        // `WinNTFileSystem_checkAccess0`: any readable attribute set means
        // readable and executable; only the READONLY attribute can deny write.
        match std::fs::metadata(path) {
            Err(_) => false,
            Ok(md) => access != FS_ACCESS_WRITE || !md.permissions().readonly(),
        }
    }
}

/// `setPermission0(File, int access, boolean enable, boolean owneronly)`.
pub(crate) fn fs_set_permission(path: &str, access: i32, enable: bool, owner_only: bool) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let md = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let amode: u32 = match access {
            FS_ACCESS_READ => {
                if owner_only {
                    0o400
                } else {
                    0o444
                }
            }
            FS_ACCESS_WRITE => {
                if owner_only {
                    0o200
                } else {
                    0o222
                }
            }
            FS_ACCESS_EXECUTE => {
                if owner_only {
                    0o100
                } else {
                    0o111
                }
            }
            _ => return false,
        };
        let old = md.permissions().mode();
        let new = if enable { old | amode } else { old & !amode };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(new)).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = owner_only;
        let md = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        if access != FS_ACCESS_WRITE {
            // Windows cannot revoke read or execute through this API; the JDK
            // native reports failure for a disable and success for an enable.
            return enable;
        }
        let mut perms = md.permissions();
        perms.set_readonly(!enable);
        std::fs::set_permissions(path, perms).is_ok()
    }
}

/// `list0(File)` — `None` maps to a Java `null`, which is what the JDK returns
/// for a path that is not a readable directory. Returning an empty array
/// instead would make `File.list()` on a plain file look like an empty
/// directory.
pub(crate) fn fs_list_dir(path: &str) -> Option<Vec<String>> {
    let entries = std::fs::read_dir(path).ok()?;
    Some(
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
    )
}

/// The `ClassId` for `java/lang/String`, for allocating a genuinely typed
/// `String[]` rather than the untyped `Object[]` that
/// `new_array(ArrayElementType::Reference, ..)` hands back.
pub(crate) fn string_class_id(ctx: &mut dyn NativeContext) -> ClassId {
    ref_array_component_id(ctx, "java/lang/String")
}

/// The `ClassId` for `java/io/File`, for the `[Ljava/io/File;`-returning
/// natives (`File.listFiles`, `File.listRoots`).
pub(crate) fn file_class_id(ctx: &mut dyn NativeContext) -> ClassId {
    ref_array_component_id(ctx, "java/io/File")
}

fn ref_array_component_id(ctx: &mut dyn NativeContext, name: &str) -> ClassId {
    ctx.ensure_class_initialized(name)
        .ok()
        .or_else(|| ctx.class_id_by_name(name))
        .unwrap_or_else(|| ClassId::new(0))
}

/// `listRoots0()` — the drive-letter bitmask (bit 0 = `A:`). Only Windows has
/// a native for this; `UnixFileSystem.listRoots()` is plain Java returning
/// `/`, so the Unix build never reaches here.
pub(crate) fn fs_list_roots_bitmask() -> i32 {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetLogicalDrives() -> u32;
        }
        // SAFETY: no arguments, no out-params — the call cannot observe or
        // corrupt anything in this process.
        (unsafe { GetLogicalDrives() }) as i32
    }
    #[cfg(not(windows))]
    {
        0
    }
}

pub(crate) fn file_read_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    // Prefer the real-JDK `getPath()` implementation when Available (correct
    // `prefixLength` + internal path for `java.io.File` loaded from modules).
    if let Ok(Some(Value::Object(Some(s)))) = ctx.invoke(
        "java/io/File",
        "getPath",
        "()Ljava/lang/String;",
        &[Value::Object(Some(this))],
    ) {
        let r = ctx.read_string(s).unwrap_or_default();
        if !r.is_empty() {
            return file_normalise_path(&r);
        }
    }
    if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "path") {
        let r = ctx.read_string(s).unwrap_or_default();
        if !r.is_empty() {
            return file_normalise_path(&r);
        }
    }
    let raw = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    // Defensive: Spring Boot's URI -> File round-trip can hand us a
    // raw URI-style `/C:/...` path on Windows when the real-JDK File
    // bytecode (which would normalise it) is bypassed. Normalise here
    // so std::fs callers see a path Rust's `Path::is_absolute` recognises.
    file_normalise_path(&raw)
}

/// Normalise a path string the way `java.io.WinNTFileSystem.normalize` does.
/// On Windows, real-JDK File.<init> strips a leading `/` before a drive
/// letter so URI-style `/C:/Users/foo` round-trips to `C:\Users\foo`. This
/// matters for `URL.toURI().getSchemeSpecificPart() -> new File(...)` —
/// the round-trip Spring Boot's fat-jar launcher relies on. On non-Windows
/// the input is returned unchanged.
#[cfg(windows)]
pub(crate) fn file_normalise_path(path: &str) -> String {
    let bytes = path.as_bytes();
    // Strip leading `/<drive>:` -> `<drive>:` (e.g. `/C:/foo` -> `C:/foo`).
    let mut normalized = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        path[1..].replace('/', "\\")
    } else {
        // Otherwise normalise forward slashes for consistency with Java's
        // canonical Windows path separator.
        path.replace('/', "\\")
    };
    // Otherwise normalise forward slashes for consistency with Java's
    // canonical Windows path separator. WinNTFileSystem.normalize also drops
    // a redundant final separator (except the drive root): this is observable
    // in Spring's config-tree location descriptions.
    while normalized.len() > 3 && normalized.ends_with('\\') {
        normalized.pop();
    }
    normalized
}

#[cfg(not(windows))]
pub(crate) fn file_normalise_path(path: &str) -> String {
    // `UnixFileSystem.normalize`: collapse runs of `/` and drop a single
    // trailing `/` (but keep the root `/` itself). `File.<init>` stores the
    // NORMALIZED string, so `getPath()`/`getAbsolutePath()` observe it.
    //
    // Leaving it un-normalized let a trailing separator survive into
    // `getAbsolutePath()`, and Spring's
    // `PathMatchingResourcePatternResolver.retrieveMatchingFiles` builds its
    // glob as `rootDir.getAbsolutePath() + "/" + subPattern` — a root
    // directory that already ended in `/` produced `.../scanned//*.txt`, which
    // matches nothing (`core.io.support.PathMatchingResourcePatternResolverTests
    // .encodedHashtagInPath` found zero files). The root came straight from
    // `new File(uri.getSchemeSpecificPart())`, whose value legitimately ends
    // in `/` for a directory URL.
    let mut out = String::with_capacity(path.len());
    let mut prev_slash = false;
    for ch in path.chars() {
        let is_slash = ch == '/';
        if !(is_slash && prev_slash) {
            out.push(ch);
        }
        prev_slash = is_slash;
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod file_normalise_tests {
    use super::file_normalise_path;

    #[cfg(not(windows))]
    #[test]
    fn unix_normalise_matches_unixfilesystem() {
        assert_eq!(file_normalise_path("/tmp/x/scanned/"), "/tmp/x/scanned");
        assert_eq!(file_normalise_path("/tmp/x//scanned"), "/tmp/x/scanned");
        assert_eq!(file_normalise_path("/tmp/x///scanned//"), "/tmp/x/scanned");
        assert_eq!(file_normalise_path("/"), "/");
        assert_eq!(file_normalise_path("//"), "/");
        assert_eq!(file_normalise_path("relative/dir/"), "relative/dir");
        assert_eq!(file_normalise_path(""), "");
    }
}

/// Resolve `new File(parent, child)` the way the JDK's
/// `WinNTFileSystem`/`UnixFileSystem.resolve` does, rather than `PathBuf::push`
/// (whose `push("")` adds a trailing separator and `push("/")` discards the
/// parent — both Java-incompatible). Strip leading/trailing separators from the
/// child and a trailing one from the parent, join with one separator, then
/// normalise. An empty child (including a lone "/" ) → just the normalised
/// parent.
pub(crate) fn file_join_parent_child(parent: &str, child: &str) -> String {
    let is_sep = |c: char| c == '/' || c == '\\';
    let child_trim = child.trim_matches(is_sep);
    if child_trim.is_empty() {
        return file_normalise_path(parent);
    }
    if parent.is_empty() {
        return file_normalise_path(child_trim);
    }
    let parent_trim = parent.trim_end_matches(is_sep);
    file_normalise_path(&format!("{parent_trim}/{child_trim}"))
}

/// Strip the Windows `\\?\` / `\\?\UNC\` extended-length prefix that
/// `std::fs::canonicalize` prepends. The real JDK's `getCanonicalPath`
/// never returns a verbatim/UNC-prefixed path; leaving `\\?\` in place
/// makes a later `File.toURI()` produce `file://?/C:/...`, an invalid URL.
pub(crate) fn strip_unc(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = p.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        p.to_string()
    }
}

/// String-based path canonicalization via Win32 `GetFullPathNameW` — makes the
/// path absolute, collapses `.`/`..`, converts `/` to `\`, and strips a trailing
/// separator, all WITHOUT opening the file (so the `cpcrypt` filesystem filter is
/// never triggered, unlike `std::fs::canonicalize`/`GetFinalPathNameByHandleW`).
/// Returns `None` for an empty input or on API error so the caller can fall back
/// to the pure-Rust lexical normalizer. Does NOT resolve symlinks — matching
/// HotSpot's largely-string-based `WinNTFileSystem.canonicalize`.
#[cfg(windows)]
pub(crate) fn win_get_full_path_name(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    extern "system" {
        fn GetFullPathNameW(
            lpFileName: *const u16,
            nBufferLength: u32,
            lpBuffer: *mut u16,
            lpFilePart: *mut *mut u16,
        ) -> u32;
    }
    // Normalize the URI-style leading `/C:` quirk and forward slashes first, so a
    // raw `/C:/foo` doesn't confuse GetFullPathName into a per-drive-relative join.
    let norm = file_normalise_path(path);
    let wide: Vec<u16> = std::ffi::OsStr::new(&norm)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        // First call (null buffer) returns the required length INCLUDING the NUL.
        let needed = GetFullPathNameW(wide.as_ptr(), 0, std::ptr::null_mut(), std::ptr::null_mut());
        if needed == 0 {
            return None;
        }
        let mut buf: Vec<u16> = vec![0u16; needed as usize];
        // Second call returns the length WITHOUT the NUL on success.
        let written = GetFullPathNameW(
            wide.as_ptr(),
            buf.len() as u32,
            buf.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        if written == 0 || written as usize >= buf.len() {
            return None;
        }
        buf.truncate(written as usize);
        let full = std::ffi::OsString::from_wide(&buf)
            .to_string_lossy()
            .into_owned();
        // GetFullPathNameW preserves a trailing separator (`foo\bar\`), but the JDK's
        // canonicalize drops it (`foo\bar`). Strip a single trailing `\` unless the
        // result is a drive root (`C:\`) or a bare root (`\`), which must keep it.
        let trimmed = if full.ends_with('\\') && !full.ends_with(":\\") && full.len() > 1 {
            full.trim_end_matches('\\').to_string()
        } else {
            full
        };
        Some(trimmed)
    }
}

/// Case-correct each *existing* path component of an absolute,
/// `GetFullPathNameW`-normalized Windows path to match the real on-disk
/// filename casing — restoring the behaviour real HotSpot's
/// `WinNTFileSystem.canonicalize` provides (it resolves the true on-disk case
/// for the existing prefix of a path, leaving any non-existent tail as given).
///
/// `GetFullPathNameW` is purely lexical: it does not query the filesystem, so
/// `getCanonicalPath("dir/D1-F1.TXT")` on a case-insensitive volume would
/// otherwise echo back the caller's requested casing verbatim, even when the
/// real on-disk file is `d1-f1.txt`. Callers that compare the canonical path
/// against the requested path to detect case mismatches (e.g. Tomcat's
/// `AbstractFileResourceSet.file()`, which rejects a request whose case
/// doesn't match the real file to guard against case-insensitive-filesystem
/// false positives) would then never observe a mismatch — silently defeating
/// the check.
///
/// This queries a per-component **targeted** `FindFirstFileW` (exact name, no
/// wildcard) against each existing ancestor directory. `FindFirstFileW`
/// enumerates directory entries; it never opens the target file itself, so —
/// like `GetFullPathNameW` — it does not engage the `cpcrypt.dll` AppCompat
/// filesystem filter that motivated moving off `std::fs::canonicalize`/
/// `GetFinalPathNameByHandleW` (see the doc comment on `win_get_full_path_name`).
#[cfg(windows)]
pub(crate) fn win_case_correct(full: &str) -> String {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[repr(C)]
    struct FileTime {
        _low: u32,
        _high: u32,
    }
    #[repr(C)]
    struct Win32FindDataW {
        _attrs: u32,
        _creation: FileTime,
        _access: FileTime,
        _write: FileTime,
        _size_high: u32,
        _size_low: u32,
        _reserved0: u32,
        _reserved1: u32,
        file_name: [u16; 260],
        _alt_name: [u16; 14],
    }
    extern "system" {
        fn FindFirstFileW(
            lp_file_name: *const u16,
            lp_find_file_data: *mut Win32FindDataW,
        ) -> *mut std::ffi::c_void;
        fn FindClose(h_find_file: *mut std::ffi::c_void) -> i32;
    }
    const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

    // Find the real on-disk name of `name` inside `dir` (both directory paths as
    // given, any case) via an exact-name `FindFirstFileW` probe. Returns `None`
    // if the entry doesn't exist.
    fn find_real_name(dir: &str, name: &str) -> Option<String> {
        let probe = format!(r"{dir}\{name}");
        let wide: Vec<u16> = std::ffi::OsStr::new(&probe)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let mut data: Win32FindDataW = std::mem::zeroed();
            let handle = FindFirstFileW(wide.as_ptr(), &mut data);
            if handle == INVALID_HANDLE_VALUE {
                return None;
            }
            FindClose(handle);
            let end = data
                .file_name
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(data.file_name.len());
            Some(
                std::ffi::OsString::from_wide(&data.file_name[..end])
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    let path = std::path::Path::new(full);
    let mut components = path.components();
    let prefix = match components.next() {
        Some(std::path::Component::Prefix(p)) => p.as_os_str().to_string_lossy().into_owned(),
        _ => return full.to_string(),
    };
    // Consume the RootDir component that follows a drive/UNC prefix, if present.
    let mut current = prefix;
    let had_root = matches!(
        components.clone().next(),
        Some(std::path::Component::RootDir)
    );
    if had_root {
        components.next();
        current.push('\\');
    }
    let mut still_existing = true;
    let mut first_comp = true;
    for comp in components {
        let std::path::Component::Normal(name) = comp else {
            continue; // GetFullPathNameW already collapsed `.`/`..`
        };
        let name = name.to_string_lossy();
        let probe_dir = current.trim_end_matches('\\').to_string();
        if !current.ends_with('\\') && !first_comp {
            current.push('\\');
        }
        first_comp = false;
        if still_existing {
            if let Some(real) = find_real_name(&probe_dir, &name) {
                current.push_str(&real);
                continue;
            }
            still_existing = false;
        }
        current.push_str(&name);
    }
    current
}

/// Canonicalize a `java.io.File` path the way `File.getCanonicalPath()` does.
///
/// `std::fs::canonicalize` only works for paths that *exist* on disk; the real
/// JDK's `getCanonicalPath` also normalizes non-existent paths — it makes them
/// absolute, collapses `.`/`..` segments, and normalizes separators. The old
/// fallback returned a raw absolute path with `..` segments intact, so a
/// containment check (`child.startsWith(parentDir)`) — as Felix's
/// `getDataFile` does — would spuriously fail.
pub(crate) fn file_canonicalize_path(path: &str) -> String {
    // JDK-faithful canonicalization cache. The real `WinNTFileSystem.canonicalize`
    // fronts a 30s `ExpiringCache` for exactly this reason: Tomcat (and most apps)
    // re-resolve the same docBase/appBase/work/temp paths on every start, and each
    // `std::fs::canonicalize` here OPENS the file → `GetFinalPathNameByHandleW`,
    // which on this box can stall for seconds inside the Crypto Pro `cpcrypt.dll`
    // filesystem filter. Caching keeps repeated `File.getCanonicalPath()` off the
    // filesystem (and the filter), cutting both per-call latency and the stall
    // variance that otherwise pushes a heavy parameterized Tomcat-churn test
    // (~144 full start/stop cycles) past its timeout.
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, (String, Instant)>>> =
        OnceLock::new();
    const TTL: Duration = Duration::from_secs(30);
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some((canon, at)) = cache.lock().unwrap().get(path) {
        if at.elapsed() < TTL {
            return canon.clone();
        }
    }
    let result = file_canonicalize_path_uncached(path);
    {
        let mut c = cache.lock().unwrap();
        // Bound memory: a long-running app may canonicalize many distinct paths.
        if c.len() > 8192 {
            c.clear();
        }
        c.insert(path.to_string(), (result.clone(), Instant::now()));
    }
    result
}

/// Resolve symlinks in the longest existing ancestor of `path` (via repeated
/// `std::fs::canonicalize` on shrinking prefixes, i.e. `realpath`), then
/// re-append whatever nonexistent trailing components were stripped off,
/// literally and lexically `.`/`..`-collapsed. Mirrors the real JDK's
/// `UnixFileSystem.canonicalize0` behavior for a path that doesn't fully
/// exist on disk (`realpath -m` semantics), unlike a pure string-only
/// normalization which never touches the filesystem and so can't resolve
/// symlinks at all. Returns `None` only if `path` can't even be made
/// absolute (no CWD available for a relative path).
#[cfg(not(windows))]
pub(crate) fn resolve_existing_ancestor_then_literal_tail(path: &str) -> Option<String> {
    let norm = file_normalise_path(path);
    let p = std::path::Path::new(&norm);
    let abs: std::path::PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(p)
    };
    use std::path::Component;
    let mut comps: Vec<std::ffi::OsString> = Vec::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                comps.pop();
            }
            Component::Normal(s) => comps.push(s.to_os_string()),
        }
    }
    // Shrink from the immediate parent of the (lexically-collapsed) full path
    // down to just "/", canonicalizing each candidate ancestor, and re-append
    // whichever trailing components had to be stripped off once one resolves.
    // (The full path itself was already tried, via `std::fs::canonicalize`,
    // by the caller before falling back to this function.) "/" itself always
    // canonicalizes, so this always terminates with `Some`.
    for split in (0..comps.len()).rev() {
        let mut candidate = std::path::PathBuf::from("/");
        for c in &comps[..split] {
            candidate.push(c);
        }
        if let Ok(canon) = std::fs::canonicalize(&candidate) {
            let mut result = canon;
            for c in &comps[split..] {
                result.push(c);
            }
            return Some(result.to_string_lossy().into_owned());
        }
    }
    Some("/".to_string())
}

pub(crate) fn file_canonicalize_path_uncached(path: &str) -> String {
    #[cfg(windows)]
    {
        // DEFAULT on Windows: canonicalize the path as a STRING via `GetFullPathNameW`,
        // which makes it absolute (against the cwd / per-drive cwd), collapses `.`/`..`,
        // normalizes separators, and drops trailing separators — all WITHOUT opening the
        // file. This mirrors HotSpot's `WinNTFileSystem.canonicalize`, which is largely
        // string-based (`GetFullPathName`) and, like us, does NOT fully resolve symlinks
        // the way `realpath`/`std::fs::canonicalize` does.
        //
        // Why this matters here: `std::fs::canonicalize` opens the file
        // (`GetFinalPathNameByHandleW` → `NtQueryInformationFile`), which on a box with
        // the Crypto Pro `cpcrypt.dll` AppCompat filesystem filter loaded triggers the
        // filter on EVERY `File.getCanonicalPath()` and can stall for ~30s globally.
        // `GetFullPathNameW` touches no file handle, so the filter is never engaged.
        // HotSpot is immune for the same reason. (cf. memory notes
        // tomcat_dohead_speed_oncpu_not_shutdown / tomcat_dohead_gc_safepoint_deadlock.)
        //
        // Escape hatch: set `CRATONVM_CANON_OPENFILE=1` to restore the old, symlink-
        // resolving, file-opening behavior if an app genuinely needs realpath semantics.
        if crate::nbflags().canon_openfile {
            if let Ok(c) = std::fs::canonicalize(path) {
                return strip_unc(&c.to_string_lossy());
            }
        } else if let Some(full) = win_get_full_path_name(path) {
            return strip_unc(&win_case_correct(&full));
        }
        // GetFullPathNameW failed (empty input / API error) — fall through to the
        // pure-Rust lexical normalization below.
    }
    #[cfg(not(windows))]
    {
        // Non-Windows has no cpcrypt-style filter hazard, so keep the symlink-resolving
        // filesystem call for existing paths.
        if let Ok(c) = std::fs::canonicalize(path) {
            return strip_unc(&c.to_string_lossy());
        }
        // `path` (or its final component(s)) doesn't exist yet — e.g. a file about
        // to be created by `WebResourceRoot.write()`/`DirResourceSet.write()`.
        // `std::fs::canonicalize` (== `realpath`) fails outright in that case, but
        // falling through to pure lexical normalization below would silently
        // UN-resolve any symlink in an EXISTING ancestor directory. That breaks any
        // `child.startsWith(canonicalBase)`-style containment check where
        // `canonicalBase` was itself computed by successfully canonicalizing an
        // existing directory through a symlink (e.g. Tomcat's
        // `AbstractFileResourceSet.file()`, or java.io.File's own canonical-path
        // contract): the base resolves through the symlink, the not-yet-existing
        // child doesn't, and the prefix check spuriously fails — a real bug,
        // confirmed via `TestWebdavServlet`'s PUT-to-a-symlinked-fixture-root
        // 201-vs-409 regression cluster. Real JDK's `UnixFileSystem.canonicalize0`
        // resolves symlinks in the longest EXISTING ancestor and appends the
        // nonexistent tail literally (`realpath -m` semantics) — do the same here.
        if let Some(resolved) = resolve_existing_ancestor_then_literal_tail(path) {
            return strip_unc(&resolved);
        }
    }
    // Lexical fallback: normalize the path as a string without touching the filesystem.
    // Make absolute against CWD, collapse `.`/`..`, normalize separators.
    let norm = file_normalise_path(path);
    let p = std::path::Path::new(&norm);
    let abs: std::path::PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    // Collapse `.` and `..` segments lexically (the prefix/root are preserved).
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop the last normal segment, but never past the root/prefix.
                match out.last() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    _ => out.push(comp),
                }
            }
            other => out.push(other),
        }
    }
    let mut result = std::path::PathBuf::new();
    for comp in out {
        result.push(comp.as_os_str());
    }
    strip_unc(&result.to_string_lossy())
}

/// Query real OS disk-space stats for the volume containing `path`, matching
/// HotSpot's `File.getTotalSpace()`/`getFreeSpace()`/`getUsableSpace()`
/// contract: returns `None` (callers report `0`) if `path` does not name an
/// existing file or directory — real HotSpot does the same rather than
/// reporting the containing volume's space for a nonexistent path (see
/// `DiskSpaceHealthIndicatorTests.whenPathDoesNotExistDiskSpaceIsDown`).
/// Returns `(total, free, usable)` in bytes on success.
#[cfg(windows)]
pub(crate) fn file_disk_space_bytes(path: &str) -> Option<(u64, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let dir_path = match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => path.to_string(),
        Ok(_) => {
            let full = win_get_full_path_name(path)?;
            std::path::Path::new(&full)
                .parent()?
                .to_string_lossy()
                .into_owned()
        }
        Err(_) => return None,
    };
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(&dir_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut usable: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut usable, &mut total, &mut free) };
    if ok == 0 {
        return None;
    }
    Some((total, free, usable))
}

#[cfg(not(windows))]
pub(crate) fn file_disk_space_bytes(path: &str) -> Option<(u64, u64, u64)> {
    if std::fs::metadata(path).is_err() {
        return None;
    }
    let c_path = std::ffi::CString::new(path).ok()?;
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        let block_size = stat.f_frsize as u64;
        let total = block_size * stat.f_blocks as u64;
        let free = block_size * stat.f_bfree as u64;
        let usable = block_size * stat.f_bavail as u64;
        Some((total, free, usable))
    }
}

/// `FileStore.getBlockSize()` — the volume's allocation/transfer unit, i.e.
/// `statvfs.f_frsize` on Unix and `GetDiskFreeSpaceW`'s bytes-per-sector on
/// Windows (exactly what the JDK's `UnixFileStore`/`WindowsFileStore` report).
/// `None` when the volume cannot be queried; callers fall back to 4 KiB.
#[cfg(windows)]
pub(crate) fn file_store_block_size(path: &str) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let full = win_get_full_path_name(path).unwrap_or_else(|| path.to_string());
    // Build the volume root (`C:\`, `\\server\share\`) GetDiskFreeSpaceW wants.
    let mut root = String::new();
    for comp in std::path::Path::new(&full).components() {
        match comp {
            std::path::Component::Prefix(prefix) => {
                root.push_str(&prefix.as_os_str().to_string_lossy());
            }
            std::path::Component::RootDir => root.push('\\'),
            _ => break,
        }
    }
    if root.is_empty() {
        return None;
    }
    if !root.ends_with('\\') {
        root.push('\\');
    }
    extern "system" {
        fn GetDiskFreeSpaceW(
            lpRootPathName: *const u16,
            lpSectorsPerCluster: *mut u32,
            lpBytesPerSector: *mut u32,
            lpNumberOfFreeClusters: *mut u32,
            lpTotalNumberOfClusters: *mut u32,
        ) -> i32;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    let mut free_clusters: u32 = 0;
    let mut total_clusters: u32 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if ok == 0 || bytes_per_sector == 0 {
        return None;
    }
    Some(u64::from(bytes_per_sector))
}

#[cfg(not(windows))]
pub(crate) fn file_store_block_size(path: &str) -> Option<u64> {
    let c_path = std::ffi::CString::new(path).ok()?;
    // SAFETY: `stat` is a POD out-parameter and `c_path` is NUL-terminated and
    // live for the call.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        let block_size = stat.f_frsize as u64;
        if block_size == 0 {
            None
        } else {
            Some(block_size)
        }
    }
}

/// `FileStore.isReadOnly()` — whether the VOLUME holding `path` is mounted
/// read-only. This is the mount flag, not a per-file permission bit: Unix
/// tests `ST_RDONLY` (value 1 on Linux and the BSDs) in `statvfs.f_flag`, the
/// same bit `sun.nio.fs.UnixFileStore.isReadOnly()` reads out of the mount
/// entry; Windows tests `FILE_READ_ONLY_VOLUME` in `GetVolumeInformationW`'s
/// filesystem flags, exactly as `sun.nio.fs.WindowsFileStore.isReadOnly()`
/// does. `None` when the volume cannot be queried at all.
#[cfg(windows)]
pub(crate) fn file_store_is_read_only(path: &str) -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;
    let full = win_get_full_path_name(path).unwrap_or_else(|| path.to_string());
    // Same volume-root derivation as `file_store_block_size` above.
    let mut root = String::new();
    for comp in std::path::Path::new(&full).components() {
        match comp {
            std::path::Component::Prefix(prefix) => {
                root.push_str(&prefix.as_os_str().to_string_lossy());
            }
            std::path::Component::RootDir => root.push('\\'),
            _ => break,
        }
    }
    if root.is_empty() {
        return None;
    }
    if !root.ends_with('\\') {
        root.push('\\');
    }
    extern "system" {
        fn GetVolumeInformationW(
            lpRootPathName: *const u16,
            lpVolumeNameBuffer: *mut u16,
            nVolumeNameSize: u32,
            lpVolumeSerialNumber: *mut u32,
            lpMaximumComponentLength: *mut u32,
            lpFileSystemFlags: *mut u32,
            lpFileSystemNameBuffer: *mut u16,
            nFileSystemNameSize: u32,
        ) -> i32;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut flags: u32 = 0;
    // SAFETY: `wide` is NUL-terminated and live for the call; every other
    // out-parameter is either null (not requested) or a live `u32`.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(flags & FILE_READ_ONLY_VOLUME != 0)
}

#[cfg(not(windows))]
pub(crate) fn file_store_is_read_only(path: &str) -> Option<bool> {
    // `ST_RDONLY` is 1 on Linux/macOS/BSD; spelled literally rather than via
    // `libc::ST_RDONLY`, which is not exported for every unix target.
    const ST_RDONLY_BIT: u64 = 1;
    let probe = if path.is_empty() { "/" } else { path };
    let c_path = std::ffi::CString::new(probe).ok()?;
    // SAFETY: `stat` is a POD out-parameter and `c_path` is NUL-terminated and
    // live for the call.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        Some(stat.f_flag as u64 & ST_RDONLY_BIT != 0)
    }
}

/// `FileSystem.getNameMax0(path)` — the longest single name component the
/// volume holding `path` accepts. Unix answers with `pathconf(_PC_NAME_MAX)`,
/// exactly like the JDK's `UnixFileSystem`. Every Windows volume type
/// (NTFS/FAT32/exFAT) reports 255, which is also the fallback when the query
/// fails or the path does not exist.
pub(crate) fn file_system_name_max(path: &str) -> i32 {
    #[cfg(unix)]
    {
        let probe = if path.is_empty() { "/" } else { path };
        if let Ok(c_path) = std::ffi::CString::new(probe) {
            // SAFETY: `c_path` is NUL-terminated and live for the call;
            // `pathconf` writes nothing back.
            let n = unsafe { libc::pathconf(c_path.as_ptr(), libc::_PC_NAME_MAX) } as i64;
            if n > 0 {
                return n.min(i64::from(i32::MAX)) as i32;
            }
        }
    }
    255
}

/// Windows file identity — `(dwVolumeSerialNumber, nFileIndexHigh,
/// nFileIndexLow)` from `GetFileInformationByHandle`, i.e. what the JDK's
/// `WindowsFileKey` wraps. `FILE_FLAG_BACKUP_SEMANTICS` is what lets a
/// DIRECTORY be opened, and directories are exactly where `fileKey()` matters
/// (`FileTreeWalker.wouldLoop`'s symlink-loop check). `dwDesiredAccess = 0` is
/// enough for a metadata query and does not need read rights on the file.
#[cfg(windows)]
fn win_file_identity(path: &str) -> Option<(u32, u32, u32)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct WinFiletime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }
    /// Mirror of Win32 `BY_HANDLE_FILE_INFORMATION` (ABI-fixed field order).
    #[repr(C)]
    struct ByHandleFileInformation {
        dw_file_attributes: u32,
        ft_creation_time: WinFiletime,
        ft_last_access_time: WinFiletime,
        ft_last_write_time: WinFiletime,
        dw_volume_serial_number: u32,
        n_file_size_high: u32,
        n_file_size_low: u32,
        n_number_of_links: u32,
        n_file_index_high: u32,
        n_file_index_low: u32,
    }
    #[link(name = "Kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            h_file: *mut std::ffi::c_void,
            lp_file_information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    // SAFETY: `info` is POD and fully written by the OS on success; the handle
    // is owned by `file` and outlives the call.
    let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as *mut _, &mut info) };
    if ok == 0 {
        return None;
    }
    Some((
        info.dw_volume_serial_number,
        info.n_file_index_high,
        info.n_file_index_low,
    ))
}

/// Allocate a new File synthetic with the given path.
pub(crate) fn file_alloc(ctx: &mut dyn NativeContext, path: &str) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/io/File", 1);
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh File (native stale-local family).
    let obj_pin = ctx.pin_native_root(obj);
    let s = ctx.create_string(path);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 0, Value::Object(Some(s)));
    ctx.unpin_native_roots(obj_pin);
    obj
}

pub fn register_phase57_file(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let file = "java/io/File";

    // <init>(String path)V
    //
    // Real JDK's File.<init>(String) runs `FileSystem.normalize(...)` which
    // on Windows strips a leading `/` before a drive letter — e.g. the
    // URI-style `/C:/Users/foo` produced by `URL.toURI().getSchemeSpecificPart()`
    // round-trips to `C:\Users\foo`. Spring Boot's fat-jar launcher relies
    // on that round-trip in Archive.create(File). Apply the same
    // normalisation so our synthetic File matches HotSpot semantics.
    r.register(file, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let normalised = file_normalise_path(&path);
        // Pin across the create_string below — a moving young GC there would
        // relocate `this` (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let s = ctx.create_string(&normalised);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field(this, 0, Value::Object(Some(s)));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });

    // <init>(String parent, String child)V
    r.register(
        file,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let parent = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let child = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // JDK `File(String parent, String child)` resolution — NOT `PathBuf::push`,
            // whose semantics differ from Java and broke `File`-based resource lookup
            // (the whole catalina.webresources cluster): `push("")` appends a trailing
            // separator (path no longer denotes the file → exists() false) and
            // `push("/")` treats the child as absolute and discards the parent. Match
            // Java: strip leading/trailing separators from the child, a trailing one
            // from the parent, join with a separator, then normalise (slash
            // conversion + collapse). Empty child → just the normalised parent.
            let path = file_join_parent_child(&parent, &child);
            // Pin across the create_string below — a moving young GC there
            // would relocate `this` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let s = ctx.create_string(&path);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, 0, Value::Object(Some(s)));
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        },
    );

    // <init>(File parent, String child)V
    r.register(
        file,
        "<init>",
        "(Ljava/io/File;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let parent_path = match args.get(1) {
                Some(Value::Object(Some(p))) => file_read_path(ctx, *p),
                _ => String::new(),
            };
            let child = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let path = file_join_parent_child(&parent_path, &child);
            // Pin across the create_string below — a moving young GC there
            // would relocate `this` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let s = ctx.create_string(&path);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, 0, Value::Object(Some(s)));
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        },
    );

    // <init>(URI)V — `new File(file:/path)`.
    //
    // The previous implementation read the URI string from raw slot 5, but
    // that slot is the `port` int in the real `java.net.URI` layout — the
    // result was an empty path. ActiveMQ's launcher locates ACTIVEMQ_HOME
    // via `new File(new URI(jarUrl).resolve(".."))`; an empty path there
    // forced a wrong `../.` fallback and broke lib/*.jar discovery.
    //
    // Read the URI's `path` field *by name* (slot-order safe), falling back
    // to parsing the cached `string` full-text field. Then apply the
    // `WinNTFileSystem.fromURIPath` transform (strip the leading `/` before
    // a drive letter, drop a trailing `/`) and normalise separators.
    r.register(file, "<init>", "(Ljava/net/URI;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let uri = match args.get(1) {
            Some(Value::Object(Some(u))) => *u,
            _ => {
                // Pin across the create_string below — a moving young GC there
                // would relocate `this` (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let s = ctx.create_string("");
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field(this, 0, Value::Object(Some(s)));
                ctx.unpin_native_roots(this_pin);
                return Ok(None);
            }
        };
        // Prefer the parsed `path` component — with the slot-collision guard:
        // synthetic 7-slot URIs (URL.toURI) answer the by-name "path" read
        // with the REAL class's field index, which lands on the raw-string
        // slot, so the value equals the ENTIRE URI text ("file:/C:/...").
        // A real hierarchical path never carries the scheme prefix; treat
        // that case as unset and parse from the raw text instead (the
        // mangled "file:\C:\..." Files emptied Gradle's ClasspathUtil walk
        // and with it every ProjectBuilder module classpath).
        let raw = crate::net_phase_e::uri_raw_string(ctx, uri);
        // Real `File(URI)` is `String p = uri.getPath();` — and `getPath()`
        // returns the DECODED path, while the `path` FIELD holds the raw,
        // still-percent-encoded one (`getRawPath()`'s value). Reading the field
        // therefore produced `/tmp/dir/resource%23test1.txt` for a file
        // genuinely named `resource#test1.txt`, so `exists()` was false for any
        // path containing a character `File.toURI()` had escaped. Spring's
        // `PathMatchingResourcePatternResolver` round-trips through
        // `new File(url.toURI())` while walking a directory, so a single `#` in
        // a resource name made the whole wildcard scan return nothing
        // (`core.io.support.PathMatchingResourcePatternResolverTests
        // .encodedHashtagInPath`).
        //
        // Ask the URI itself, so the decoding rules stay the JDK's. The field
        // read remains as the fallback for the synthetic 7-slot URIs described
        // below, whose `getPath()` may not be wired.
        let mut path = match ctx.invoke_virtual(uri, "getPath", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx
                .read_string(s)
                .filter(|v| !v.is_empty() && *v != raw)
                .unwrap_or_default(),
            _ => String::new(),
        };
        if path.is_empty() {
            path = match ctx.get_field_by_name(uri, "path") {
                Value::Object(Some(s)) => ctx
                    .read_string(s)
                    .filter(|v| !v.is_empty() && *v != raw)
                    .unwrap_or_default(),
                _ => String::new(),
            };
        }
        if path.is_empty() {
            // Parse from the full URI text:
            //   scheme:[//authority]path[?query][#fragment]
            if !raw.is_empty() {
                let after_scheme = match raw.find(':') {
                    Some(i) => &raw[i + 1..],
                    None => &raw[..],
                };
                let body = if let Some(rest) = after_scheme.strip_prefix("//") {
                    let slash = rest.find('/').unwrap_or(rest.len());
                    &rest[slash..]
                } else {
                    after_scheme
                };
                path = body
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
        }
        // WinNTFileSystem.fromURIPath: `/C:/foo/` -> `C:/foo`.
        let mut p = path;
        let chars: Vec<char> = p.chars().collect();
        if chars.len() > 2 && chars[0] == '/' && chars[2] == ':' {
            p = p[1..].to_string();
            if p.len() > 3 && p.ends_with('/') {
                p.pop();
            }
        } else if p.len() > 1 && p.ends_with('/') {
            p.pop();
        }
        // Normalise to platform separators / collapse `.` `..` segments.
        let normalised = file_normalise_path(&p);
        let s = ctx.create_string(&normalised);
        ctx.set_field(this, 0, Value::Object(Some(s)));
        Ok(None)
    });

    // --- Path accessors ---
    // `getPath()`/`toString()` render the OS-native separator. `File.<init>`
    // already normalises field 0 (so a normally-constructed File holds '\' on
    // Windows), but a File minted from `Path.toFile()` keeps the Path's '/'
    // internal form — normalise on read so both paths agree with HotSpot
    // (no-op on Unix; field 0 holds no jar-FS sentinel).
    r.register(file, "getPath", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            other => return Ok(Some(other)),
        };
        let s = ctx.create_string(&file_normalise_path(&raw));
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(file, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            other => return Ok(Some(other)),
        };
        let s = ctx.create_string(&file_normalise_path(&raw));
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(file, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let s = ctx.create_string(&name);
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(file, "getParent", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        match std::path::Path::new(&path).parent() {
            Some(p) => {
                let pstr = p.to_string_lossy().into_owned();
                if pstr.is_empty() {
                    Ok(Some(Value::Object(None)))
                } else {
                    let s = ctx.create_string(&pstr);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
            None => Ok(Some(Value::Object(None))),
        }
    });
    r.register(file, "getParentFile", "()Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        match std::path::Path::new(&path).parent() {
            Some(p) => {
                let pstr = p.to_string_lossy().into_owned();
                if pstr.is_empty() {
                    Ok(Some(Value::Object(None)))
                } else {
                    Ok(Some(Value::Object(Some(file_alloc(ctx, &pstr)))))
                }
            }
            None => Ok(Some(Value::Object(None))),
        }
    });
    r.register(
        file,
        "getAbsolutePath",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let p = std::path::Path::new(&path);
            let abs = if p.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(&path).to_string_lossy().into_owned())
                    .unwrap_or(path)
            };
            // java.io.File normalizes a trailing separator on ordinary paths
            // (`new File("/tmp/").getAbsolutePath()` is `/tmp`). Keeping it
            // made Keycloak persist kc.home.dir with a trailing slash while
            // Path-based config resolution returned the normalized form.
            let abs = p57_trim_file_trailing_separator(&abs);
            let s = ctx.create_string(&abs);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(file, "getAbsoluteFile", "()Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let p = std::path::Path::new(&path);
        let abs = if p.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path).to_string_lossy().into_owned())
                .unwrap_or(path)
        };
        let abs = p57_trim_file_trailing_separator(&abs);
        Ok(Some(Value::Object(Some(file_alloc(ctx, &abs)))))
    });
    // Strip the Windows `\\?\` extended-length prefix that
    // `std::fs::canonicalize` prepends. The real JDK's `getCanonicalPath`
    // never returns a verbatim/UNC-prefixed path; leaving `\\?\` in place
    // makes a later `File.toURI()` produce `file://?/C:/...`, an invalid
    // URL that breaks Tomcat's `ClassLoaderFactory.buildClassLoaderUrl`.
    r.register(
        file,
        "getCanonicalPath",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let canonical = file_canonicalize_path(&path);
            let s = ctx.create_string(&canonical);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(file, "getCanonicalFile", "()Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let canonical = file_canonicalize_path(&path);
        Ok(Some(Value::Object(Some(file_alloc(ctx, &canonical)))))
    });
    r.register(file, "isAbsolute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let abs = std::path::Path::new(&path).is_absolute();
        Ok(Some(Value::Int(if abs { 1 } else { 0 })))
    });

    // --- Metadata (existence, type, permissions) ---
    r.register(file, "exists", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let exists = std::path::Path::new(&path).exists();
        if crate::nbflags().dbg_sbload {
            eprintln!("[DBG_SBLOAD] File.exists() path={:?} -> {}", path, exists);
        }
        Ok(Some(Value::Int(if exists { 1 } else { 0 })))
    });
    r.register(file, "isFile", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let result = std::fs::metadata(&path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        Ok(Some(Value::Int(if result { 1 } else { 0 })))
    });
    r.register(file, "isDirectory", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let result = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if crate::nbflags().dbg_sbload {
            eprintln!(
                "[DBG_SBLOAD] File.isDirectory() path={:?} -> {}",
                path, result
            );
        }
        Ok(Some(Value::Int(if result { 1 } else { 0 })))
    });
    r.register(file, "isHidden", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        // Cross-platform: treat files with leading '.' as hidden
        let hidden = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().starts_with('.'))
            .unwrap_or(false);
        Ok(Some(Value::Int(if hidden { 1 } else { 0 })))
    });
    r.register(file, "canRead", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        // Approximate: if we can open it for reading or get metadata, it's readable
        let readable = std::fs::metadata(&path).is_ok();
        Ok(Some(Value::Int(if readable { 1 } else { 0 })))
    });
    r.register(file, "canWrite", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let writable = std::fs::metadata(&path)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false);
        Ok(Some(Value::Int(if writable { 1 } else { 0 })))
    });
    r.register(file, "canExecute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = std::fs::metadata(&path)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            Ok(Some(Value::Int(if executable { 1 } else { 0 })))
        }
        #[cfg(not(unix))]
        {
            // On Windows, treat existing files as executable
            let exists = std::path::Path::new(&path).exists();
            Ok(Some(Value::Int(if exists { 1 } else { 0 })))
        }
    });
    r.register(file, "length", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let size = std::fs::metadata(&path)
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        Ok(Some(Value::Long(size)))
    });
    r.register(file, "lastModified", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let millis = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(Some(Value::Long(millis)))
    });

    // --- Mutations ---
    r.register(file, "delete", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let p = std::path::Path::new(&path);
        let ok = if p.is_dir() {
            std::fs::remove_dir(p).is_ok()
        } else {
            std::fs::remove_file(p).is_ok()
        };
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "mkdir", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let ok = std::fs::create_dir(&path).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "mkdirs", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let ok = std::fs::create_dir_all(&path).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "createNewFile", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Some(Value::Int(1))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Some(Value::Int(0))),
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    });
    r.register(file, "renameTo", "(Ljava/io/File;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let src = file_read_path(ctx, this);
        let dst = match args.get(1) {
            Some(Value::Object(Some(f))) => file_read_path(ctx, *f),
            _ => return Ok(Some(Value::Int(0))),
        };
        let ok = std::fs::rename(&src, &dst).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "setLastModified", "(J)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let millis = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        Ok(Some(Value::Int(if set_file_mtime_millis(&path, millis) {
            1
        } else {
            0
        })))
    });
    r.register(file, "setReadable", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let _path = file_read_path(ctx, this);
        let _readable = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        // Best-effort: if file exists, return true. Real permission change is platform-dependent.
        let ok = std::fs::metadata(&_path).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "setReadable", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let _path = file_read_path(ctx, this);
        let ok = std::fs::metadata(&_path).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "setWritable", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let writable = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let ok = std::fs::metadata(&path)
            .and_then(|meta| {
                let mut perms = meta.permissions();
                perms.set_readonly(!writable);
                std::fs::set_permissions(&path, perms)
            })
            .is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "setWritable", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let writable = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let ok = std::fs::metadata(&path)
            .and_then(|meta| {
                let mut perms = meta.permissions();
                perms.set_readonly(!writable);
                std::fs::set_permissions(&path, perms)
            })
            .is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "setExecutable", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let _path = file_read_path(ctx, this);
        let _exec = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        // On unix, toggle 0o100 bit; on Windows, no-op (treat as success)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ok = std::fs::metadata(&_path)
                .and_then(|meta| {
                    let mut perms = meta.permissions();
                    let mode = perms.mode();
                    let new_mode = if _exec { mode | 0o111 } else { mode & !0o111 };
                    perms.set_mode(new_mode);
                    std::fs::set_permissions(&_path, perms)
                })
                .is_ok();
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        }
        #[cfg(not(unix))]
        {
            let ok = std::fs::metadata(&_path).is_ok();
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        }
    });
    r.register(file, "setExecutable", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let _path = file_read_path(ctx, this);
        let ok = std::fs::metadata(&_path).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });
    r.register(file, "setReadOnly", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let ok = std::fs::metadata(&path)
            .and_then(|meta| {
                let mut perms = meta.permissions();
                perms.set_readonly(true);
                std::fs::set_permissions(&path, perms)
            })
            .is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    });

    // --- Listing ---
    r.register(file, "list", "()[Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                // A typed component class, not the untyped `Object[]` that
                // `ArrayElementType::Reference` produces — the descriptor's array
                // type is what the caller's assignment checkcasts against.
                let component = string_class_id(ctx);
                let arr = ctx.new_ref_array(component, names.len());
                // Pin across the create_strings below — a moving young GC
                // there would relocate the fresh array (native stale-local
                // family).
                let arr_pin = ctx.pin_native_root(arr);
                for (i, name) in names.iter().enumerate() {
                    let s = ctx.create_string(name);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, Value::Object(Some(s)));
                }
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(arr_pin);
                Ok(Some(Value::Object(Some(arr))))
            }
            Err(_) => Ok(Some(Value::Object(None))),
        }
    });
    r.register(file, "listFiles", "()[Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let paths: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect();
                // A typed component class, not the untyped `Object[]` that
                // `ArrayElementType::Reference` produces — the descriptor's array
                // type is what the caller's assignment checkcasts against.
                let component = file_class_id(ctx);
                let arr = ctx.new_ref_array(component, paths.len());
                // Pin across the File allocs below — a moving young GC there
                // would relocate the fresh array (native stale-local family).
                let arr_pin = ctx.pin_native_root(arr);
                for (i, p) in paths.iter().enumerate() {
                    let f = file_alloc(ctx, p);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, Value::Object(Some(f)));
                }
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(arr_pin);
                Ok(Some(Value::Object(Some(arr))))
            }
            Err(_) => Ok(Some(Value::Object(None))),
        }
    });
    // listFiles(FileFilter) — invoke filter.accept(File) for each entry
    r.register(
        file,
        "listFiles",
        "(Ljava/io/FileFilter;)[Ljava/io/File;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let filter = args.get(1).copied().unwrap_or(Value::Object(None));
            let entries: Vec<String> = match std::fs::read_dir(&path) {
                Ok(iter) => iter
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect(),
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            // Pin across the File allocs / filter callbacks below — a moving
            // young GC there would relocate them (native stale-local family);
            // each fresh File is pinned as it materialises.
            let filter_pin = pinned_object_value(ctx, filter);
            let mut first_pin = filter_pin.map(|(h, _)| h);
            let mut accepted: Vec<(usize, ObjectRef)> = Vec::new();
            for entry_path in &entries {
                let file_obj = file_alloc(ctx, entry_path);
                let file_pin = ctx.pin_native_root(file_obj);
                if first_pin.is_none() {
                    first_pin = Some(file_pin);
                }
                let accept = match read_pinned_object_value(ctx, filter_pin, filter) {
                    Value::Object(Some(f)) => {
                        match ctx.invoke_virtual(
                            f,
                            "accept",
                            "(Ljava/io/File;)Z",
                            &[Value::Object(Some(file_obj))],
                        ) {
                            Ok(Some(Value::Int(v))) => v != 0,
                            _ => true, // on failure, include
                        }
                    }
                    _ => true, // null filter accepts everything
                };
                if accept {
                    accepted.push((file_pin, file_obj));
                }
            }
            // A typed component class, not the untyped `Object[]` that
            // `ArrayElementType::Reference` produces — the descriptor's array
            // type is what the caller's assignment checkcasts against.
            let component = file_class_id(ctx);
            let arr = ctx.new_ref_array(component, accepted.len());
            for (i, (pin, orig)) in accepted.iter().enumerate() {
                let f = ctx.read_native_pin(*pin, *orig);
                ctx.set_array_element(arr, i, Value::Object(Some(f)));
            }
            if let Some(h) = first_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    // listFiles(FilenameFilter) — invoke filter.accept(File dir, String name)
    r.register(
        file,
        "listFiles",
        "(Ljava/io/FilenameFilter;)[Ljava/io/File;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let filter = args.get(1).copied().unwrap_or(Value::Object(None));
            let entries: Vec<(String, String)> = match std::fs::read_dir(&path) {
                Ok(iter) => iter
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let full = e.path().to_string_lossy().into_owned();
                        let name = e.file_name().to_string_lossy().into_owned();
                        (full, name)
                    })
                    .collect(),
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            // Pin across the File allocs / filter callbacks below — a moving
            // young GC there would relocate them (native stale-local family);
            // each fresh File is pinned as it materialises.
            let this_pin = ctx.pin_native_root(this);
            let filter_pin = pinned_object_value(ctx, filter);
            let mut accepted: Vec<(usize, ObjectRef)> = Vec::new();
            for (full, name) in &entries {
                let accept = match read_pinned_object_value(ctx, filter_pin, filter) {
                    Value::Object(Some(f)) => {
                        let name_str = ctx.create_string(name);
                        let this = ctx.read_native_pin(this_pin, this);
                        match ctx.invoke_virtual(
                            f,
                            "accept",
                            "(Ljava/io/File;Ljava/lang/String;)Z",
                            &[Value::Object(Some(this)), Value::Object(Some(name_str))],
                        ) {
                            Ok(Some(Value::Int(v))) => v != 0,
                            _ => true,
                        }
                    }
                    _ => true,
                };
                if accept {
                    let f_obj = file_alloc(ctx, full);
                    accepted.push((ctx.pin_native_root(f_obj), f_obj));
                }
            }
            // A typed component class, not the untyped `Object[]` that
            // `ArrayElementType::Reference` produces — the descriptor's array
            // type is what the caller's assignment checkcasts against.
            let component = file_class_id(ctx);
            let arr = ctx.new_ref_array(component, accepted.len());
            for (i, (pin, orig)) in accepted.iter().enumerate() {
                let f = ctx.read_native_pin(*pin, *orig);
                ctx.set_array_element(arr, i, Value::Object(Some(f)));
            }
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    // list(FilenameFilter) — string array variant
    r.register(
        file,
        "list",
        "(Ljava/io/FilenameFilter;)[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let filter = args.get(1).copied().unwrap_or(Value::Object(None));
            let entries: Vec<String> = match std::fs::read_dir(&path) {
                Ok(iter) => iter
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect(),
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            // Pin across the filter callbacks below — a moving young GC there
            // would relocate `this`/`filter` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let filter_pin = pinned_object_value(ctx, filter);
            let mut accepted: Vec<String> = Vec::new();
            for name in &entries {
                let accept = match read_pinned_object_value(ctx, filter_pin, filter) {
                    Value::Object(Some(f)) => {
                        let name_str = ctx.create_string(name);
                        let this = ctx.read_native_pin(this_pin, this);
                        match ctx.invoke_virtual(
                            f,
                            "accept",
                            "(Ljava/io/File;Ljava/lang/String;)Z",
                            &[Value::Object(Some(this)), Value::Object(Some(name_str))],
                        ) {
                            Ok(Some(Value::Int(v))) => v != 0,
                            _ => true,
                        }
                    }
                    _ => true,
                };
                if accept {
                    accepted.push(name.clone());
                }
            }
            ctx.unpin_native_roots(this_pin);
            // A typed component class, not the untyped `Object[]` that
            // `ArrayElementType::Reference` produces — the descriptor's array
            // type is what the caller's assignment checkcasts against.
            let component = string_class_id(ctx);
            let arr = ctx.new_ref_array(component, accepted.len());
            let arr_pin = ctx.pin_native_root(arr);
            for (i, name) in accepted.iter().enumerate() {
                let s = ctx.create_string(name);
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // --- Disk space (real OS query; 0 for a path that does not exist, matching
    // HotSpot's WinNTFileSystem/UnixFileSystem contract) ---
    r.register(file, "getFreeSpace", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let free = file_disk_space_bytes(&path).map_or(0, |(_, free, _)| free);
        Ok(Some(Value::Long(free as i64)))
    });
    r.register(file, "getTotalSpace", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let total = file_disk_space_bytes(&path).map_or(0, |(total, _, _)| total);
        Ok(Some(Value::Long(total as i64)))
    });
    r.register(file, "getUsableSpace", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let usable = file_disk_space_bytes(&path).map_or(0, |(_, _, usable)| usable);
        Ok(Some(Value::Long(usable as i64)))
    });

    // --- Equality / comparison ---
    r.register(file, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let p1 = file_read_path(ctx, this);
        let p2 = file_read_path(ctx, other);
        Ok(Some(Value::Int(if p1 == p2 { 1 } else { 0 })))
    });
    r.register(file, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        // Java String.hashCode() algorithm
        let mut h: i32 = 0;
        for b in path.bytes() {
            h = h.wrapping_mul(31).wrapping_add(b as i32);
        }
        Ok(Some(Value::Int(h)))
    });
    r.register(file, "compareTo", "(Ljava/io/File;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let p1 = file_read_path(ctx, this);
        let p2 = file_read_path(ctx, other);
        Ok(Some(Value::Int(p1.cmp(&p2) as i32)))
    });

    // toPath and toURI
    //
    // Validate the path first — real `File.toPath()` delegates to
    // `FileSystems.getDefault().getPath(...)`, which throws
    // `InvalidPathException` for Windows-illegal syntax (e.g. a bare colon
    // outside a drive specifier, as in Spring's `ping:foo`
    // `ProtocolResolver` probe: `GenericApplicationContextTests.
    // getResourceWithCustomResourceLoader` relies on `FileSystemResource`'s
    // `this.file.toPath()` throwing for exactly this). See
    // `p57_validate_windows_path` (registered alongside `Paths.get` above).
    // Allocate through `p57_alloc_path` so File paths use the same internal
    // separator canonicalisation as `Paths.get(...)`; Windows `File` display
    // paths contain `\`, but `Path.toUri()` must render `/`, not `%5C`.
    r.register(file, "toPath", "()Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        if let Err(reason) = p57_validate_windows_path(&path) {
            return match ctx.new_object("java/nio/file/InvalidPathException") {
                Ok(Some(Value::Object(Some(exc)))) => {
                    let input_str = ctx.create_string(&path);
                    let reason_str = ctx.create_string(reason);
                    let _ = ctx.invoke(
                        "java/nio/file/InvalidPathException",
                        "<init>",
                        "(Ljava/lang/String;Ljava/lang/String;)V",
                        &[
                            Value::Object(Some(exc)),
                            Value::Object(Some(input_str)),
                            Value::Object(Some(reason_str)),
                        ],
                    );
                    Err(MethodCallFailed::ExceptionThrown(exc))
                }
                _ => Err(RuntimeError::IllegalArgumentException {
                    message: format!("{reason}: {path}"),
                }
                .into()),
            };
        }
        Ok(Some(Value::Object(Some(p57_alloc_path(ctx, &path)))))
    });
    r.register(file, "toURI", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        // File.toURI() operates on getAbsoluteFile(): a RELATIVE File must be
        // resolved against the working directory first. Skipping that turned
        // `new File("runner").toURI()` into `file:/runner/` — a nonexistent
        // root path that silently dropped relative classpath entries from
        // Gradle's ClasspathUtil walk.
        let path = {
            let p = std::path::Path::new(&path);
            if p.is_absolute() {
                path.clone()
            } else {
                match std::env::current_dir() {
                    Ok(cwd) => cwd.join(p).to_string_lossy().to_string(),
                    Err(_) => path.clone(),
                }
            }
        };
        // Per `File.toURI()`, the result is `new URI("file", null, slashify(absPath, isDir), null)`,
        // which renders as `file:/C:/...` — a SINGLE slash before the path
        // (no `//authority`). Emitting `file://` + `/C:/...` produced the
        // malformed `file:///C:/...` whose `URI.toURL()` returned null and
        // broke every `URLClassLoader` built from `File.toURI().toURL()`.
        let mut dir_path = path.replace('\\', "/");
        if !dir_path.starts_with('/') {
            dir_path = format!("/{dir_path}");
        }
        let is_dir = std::path::Path::new(&path).is_dir();
        // Directory URIs end with a trailing slash, matching `slashify(...)`.
        if is_dir && !dir_path.ends_with('/') {
            dir_path.push('/');
        }
        // RFC 2396 percent-encoding, matching `sun.net.www.ParseUtil.encodePath`
        // used by `File.toURI()`. Without this, special path chars (`^`, space,
        // `#`, `?`, …) leaked into the URI literally, so `File.toURI()` returned
        // `file:/a b/c` instead of `file:/a%20b/c` (Tomcat UriUtil tests; any
        // `new URI(File.toURI().toString())` round-trip on such paths threw).
        let encoded = encode_file_uri_path(&dir_path);
        let full = format!("file:{encoded}");
        // Allocate a real URI and populate named + positional fields so both
        // `URI` natives and any real-JDK bytecode see consistent state.
        let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 7);
        // Pin across the parse/store helpers below (they create strings) — a
        // moving young GC there would relocate the fresh URI (native
        // stale-local family).
        let uri_pin = ctx.pin_native_root(uri);
        crate::url_parse(ctx, uri, &full);
        let uri = ctx.read_native_pin(uri_pin, uri);
        crate::uri_store_named(ctx, uri, &full);
        let uri = ctx.read_native_pin(uri_pin, uri);
        ctx.unpin_native_roots(uri_pin);
        Ok(Some(Value::Object(Some(uri))))
    });

    // Static helpers
    r.register(file, "listRoots", "()[Ljava/io/File;", |ctx, _args| {
        #[cfg(windows)]
        let roots: Vec<String> = {
            let mut out = Vec::new();
            for letter in b'A'..=b'Z' {
                let root = format!("{}:\\", letter as char);
                if std::path::Path::new(&root).exists() {
                    out.push(root);
                }
            }
            if out.is_empty() {
                out.push("C:\\".to_string());
            }
            out
        };
        #[cfg(not(windows))]
        let roots: Vec<String> = vec!["/".to_string()];
        // A typed component class, not the untyped `Object[]` that
        // `ArrayElementType::Reference` produces — the descriptor's array
        // type is what the caller's assignment checkcasts against.
        let component = file_class_id(ctx);
        let arr = ctx.new_ref_array(component, roots.len());
        // Pin across the File allocs below — a moving young GC there would
        // relocate the fresh array (native stale-local family).
        let arr_pin = ctx.pin_native_root(arr);
        for (i, r) in roots.iter().enumerate() {
            let f = file_alloc(ctx, r);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(f)));
        }
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.unpin_native_roots(arr_pin);
        Ok(Some(Value::Object(Some(arr))))
    });
    r.register(
        file,
        "createTempFile",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/File;",
        |ctx, args| {
            let prefix = match args.get(0) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_else(|| "tmp".into()),
                _ => "tmp".into(),
            };
            let suffix = match args.get(1) {
                Some(Value::Object(Some(s))) => {
                    ctx.read_string(*s).unwrap_or_else(|| ".tmp".into())
                }
                _ => ".tmp".into(),
            };
            let tmp_dir = jdk_temp_dir(ctx.get_system_property("java.io.tmpdir"));
            let full = jdk_create_temp_file(&tmp_dir, &prefix, &suffix)?;
            Ok(Some(Value::Object(Some(file_alloc(ctx, &full)))))
        },
    );
    r.register(
        file,
        "createTempFile",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/io/File;)Ljava/io/File;",
        |ctx, args| {
            let prefix = match args.get(0) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_else(|| "tmp".into()),
                _ => "tmp".into(),
            };
            let suffix = match args.get(1) {
                Some(Value::Object(Some(s))) => {
                    ctx.read_string(*s).unwrap_or_else(|| ".tmp".into())
                }
                _ => ".tmp".into(),
            };
            let dir_path = match args.get(2) {
                Some(Value::Object(Some(f))) => std::path::PathBuf::from(file_read_path(ctx, *f)),
                _ => jdk_temp_dir(ctx.get_system_property("java.io.tmpdir")),
            };
            let full = jdk_create_temp_file(&dir_path, &prefix, &suffix)?;
            Ok(Some(Value::Object(Some(file_alloc(ctx, &full)))))
        },
    );
    // Was a documented "no actual tracking" no-op: the file was never deleted
    // and nothing told the caller so. Anything relying on it for cleanup (temp
    // files from `File.createTempFile(...).deleteOnExit()`, unpacked native
    // libraries, scratch DB files) leaked on every run. Record the path and
    // delete it from a C-runtime `atexit` handler, which is what
    // `DeleteOnExitHook` does from the JDK's shutdown hook. Deletion is
    // reverse-registration order, matching the JDK.
    r.register(file, "deleteOnExit", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        if !path.is_empty() {
            delete_on_exit_register(path);
        }
        Ok(None)
    });
    r.register(file, "separator", "Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string(std::path::MAIN_SEPARATOR_STR);
        Ok(Some(Value::Object(Some(s))))
    });
    // KEEP: `File.separatorChar` is a compile-time platform constant in the
    // real JDK too; `MAIN_SEPARATOR` is the genuine host value, not a stand-in.
    r.register(file, "separatorChar", "C", |_ctx, _args| {
        Ok(Some(Value::Int(std::path::MAIN_SEPARATOR as i32)))
    });
    r.register(file, "pathSeparator", "Ljava/lang/String;", |ctx, _args| {
        #[cfg(windows)]
        let sep = ";";
        #[cfg(not(windows))]
        let sep = ":";
        let s = ctx.create_string(sep);
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(file, "pathSeparatorChar", "C", |_ctx, _args| {
        #[cfg(windows)]
        let sep = ';' as i32;
        #[cfg(not(windows))]
        let sep = ':' as i32;
        Ok(Some(Value::Int(sep)))
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// java.nio.channels.FileChannel — 1-field synthetic (fd_id=0 Int)
// Real file operations via fd_table (same API as RandomAccessFile)
// ---------------------------------------------------------------------------

pub(crate) fn register_phase57_file_channel(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let fc = "java/nio/channels/FileChannel";

    // position()J
    r.register(fc, "position", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        if fd_id < 0 {
            return Err(RuntimeError::IOException {
                message: "Channel closed".into(),
            }
            .into());
        }
        let pos =
            ctx.fd_table()
                .rw_position(fd_id as u32)
                .map_err(|e| RuntimeError::IOException {
                    message: e.to_string(),
                })?;
        Ok(Some(Value::Long(pos as i64)))
    });

    // position(J)FileChannel — returns this
    r.register(
        fc,
        "position",
        "(J)Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
            let pos = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            if fd_id >= 0 {
                ctx.fd_table()
                    .rw_seek(fd_id as u32, std::io::SeekFrom::Start(pos.max(0) as u64))
                    .map_err(|e| RuntimeError::IOException {
                        message: e.to_string(),
                    })?;
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // size()J
    r.register(fc, "size", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        if fd_id < 0 {
            return Err(RuntimeError::IOException {
                message: "Channel closed".into(),
            }
            .into());
        }
        let sz = ctx
            .fd_table()
            .file_size(fd_id as u32)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(Some(Value::Long(sz as i64)))
    });

    // truncate(J)FileChannel
    r.register(
        fc,
        "truncate",
        "(J)Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
            let new_len = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            if fd_id >= 0 {
                ctx.fd_table()
                    .rw_set_length(fd_id as u32, new_len.max(0) as u64)
                    .map_err(|e| RuntimeError::IOException {
                        message: e.to_string(),
                    })?;
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // read(ByteBuffer)I
    r.register(fc, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        if fd_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Int(-1))),
        };
        // ByteBuffer: field 0=backing array, field 1=position, field 2=limit
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut buf = vec![0u8; remaining];
        match ctx.fd_table().rw_read(fd_id as u32, &mut buf) {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                    for i in 0..n {
                        ctx.set_array_element(arr, bb_pos + i, Value::Int(buf[i] as i8 as i32));
                    }
                }
                ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
                Ok(Some(Value::Int(n as i32)))
            }
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    });

    // write(ByteBuffer)I
    r.register(fc, "write", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        if fd_id < 0 {
            return Err(RuntimeError::IOException {
                message: "Channel closed".into(),
            }
            .into());
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Int(0))),
        };
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut data = vec![0u8; remaining];
        if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
            for i in 0..remaining {
                data[i] = ctx.get_array_element(arr, bb_pos + i).as_int().unwrap_or(0) as u8;
            }
        }
        let n = ctx.fd_table().rw_write(fd_id as u32, &data).map_err(|e| {
            RuntimeError::IOException {
                message: e.to_string(),
            }
        })?;
        ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
        Ok(Some(Value::Int(n as i32)))
    });

    // read(ByteBuffer, long position)I — positional read
    r.register(fc, "read", "(Ljava/nio/ByteBuffer;J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        let position = match args.get(2) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        if fd_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut buf = vec![0u8; remaining];
        match ctx
            .fd_table()
            .pread_at(fd_id as u32, &mut buf, position.max(0) as u64)
        {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                    for i in 0..n {
                        ctx.set_array_element(arr, bb_pos + i, Value::Int(buf[i] as i8 as i32));
                    }
                }
                ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
                Ok(Some(Value::Int(n as i32)))
            }
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    });

    // write(ByteBuffer, long position)I — positional write
    r.register(fc, "write", "(Ljava/nio/ByteBuffer;J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        let position = match args.get(2) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        if fd_id < 0 {
            return Err(RuntimeError::IOException {
                message: "Channel closed".into(),
            }
            .into());
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Int(0))),
        };
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut data = vec![0u8; remaining];
        if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
            for i in 0..remaining {
                data[i] = ctx.get_array_element(arr, bb_pos + i).as_int().unwrap_or(0) as u8;
            }
        }
        let n = ctx
            .fd_table()
            .pwrite_at(fd_id as u32, &data, position.max(0) as u64)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
        Ok(Some(Value::Int(n as i32)))
    });

    // transferTo(long position, long count, WritableByteChannel target)J
    r.register(
        fc,
        "transferTo",
        "(JJLjava/nio/channels/WritableByteChannel;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
            let position = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let count = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let target = match args.get(3) {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(Some(Value::Long(0))),
            };
            if fd_id < 0 || count <= 0 {
                return Ok(Some(Value::Long(0)));
            }
            // BUG nb-phases-late(2): a huge `count` (up to Long.MAX_VALUE) was fed
            // straight into `vec![0u8; count as usize]` → exabyte allocation abort.
            // The JDK clamps the transfer to the bytes actually available
            // (file_size - position) and streams through a bounded buffer. Clamp
            // `count` to what remains in the source file, then loop in chunks so
            // the per-iteration allocation is bounded regardless of `count`.
            let position = position.max(0);
            let file_size = ctx.fd_table().file_size(fd_id as u32).unwrap_or(0) as i64;
            let available = (file_size - position).max(0);
            // Clamp to the bytes available in the source file when we have a real
            // size; for non-regular/unknown-size fds (file_size==0) fall back to the
            // caller's `count` and let the chunked read loop stop at EOF — this keeps
            // the per-iteration allocation bounded either way.
            let mut to_transfer = if file_size > 0 {
                count.min(available)
            } else {
                count
            };
            if to_transfer <= 0 {
                return Ok(Some(Value::Long(0)));
            }
            // Bounded streaming buffer (8 MiB) — matches the JDK's chunked fallback
            // when a true zero-copy sendfile is unavailable.
            const FC_XFER_CHUNK: i64 = 8 * 1024 * 1024;
            // Pin across the per-chunk allocs / write callbacks below — a
            // moving young GC there would relocate `target` (native
            // stale-local family).
            let target_pin = ctx.pin_native_root(target);
            let mut total_written: i64 = 0;
            let mut cur_pos = position;
            while to_transfer > 0 {
                let chunk = to_transfer.min(FC_XFER_CHUNK) as usize;
                let mut buf = vec![0u8; chunk];
                let n = ctx
                    .fd_table()
                    .pread_at(fd_id as u32, &mut buf, cur_pos as u64)
                    .unwrap_or(0);
                if n == 0 {
                    break;
                }
                // Wrap in a ByteBuffer and call target.write(ByteBuffer)
                let byte_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, n);
                for i in 0..n {
                    ctx.set_array_element(byte_arr, i, Value::Int(buf[i] as i8 as i32));
                }
                let byte_arr_pin = ctx.pin_native_root(byte_arr);
                let bb = alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 3);
                let byte_arr = ctx.read_native_pin(byte_arr_pin, byte_arr);
                ctx.set_field(bb, 0, Value::Object(Some(byte_arr)));
                ctx.set_field(bb, 1, Value::Int(0));
                ctx.set_field(bb, 2, Value::Int(n as i32));
                let target_cur = ctx.read_native_pin(target_pin, target);
                let written = match ctx.invoke_virtual(
                    target_cur,
                    "write",
                    "(Ljava/nio/ByteBuffer;)I",
                    &[Value::Object(Some(bb))],
                ) {
                    Ok(Some(Value::Int(w))) if w >= 0 => w as i64,
                    _ => n as i64,
                };
                ctx.unpin_native_roots(byte_arr_pin);
                total_written += written;
                cur_pos += n as i64;
                to_transfer -= n as i64;
                // Short write from the target or short read from the source: stop,
                // mirroring the JDK which returns the bytes transferred so far.
                if (written as usize) < n || n < chunk {
                    break;
                }
            }
            ctx.unpin_native_roots(target_pin);
            Ok(Some(Value::Long(total_written)))
        },
    );

    // transferFrom(ReadableByteChannel src, long position, long count)J
    r.register(
        fc,
        "transferFrom",
        "(Ljava/nio/channels/ReadableByteChannel;JJ)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
            let src = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Long(0))),
            };
            let position = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let count = match args.get(3) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            if fd_id < 0 || count <= 0 {
                return Ok(Some(Value::Long(0)));
            }
            // BUG nb-phases-late(2): `count` (a long) was used to size the
            // ByteBuffer via `new_array(count as usize)` (exabyte alloc abort for a
            // huge/Long.MAX_VALUE count) AND passed to the buffer's limit as
            // `Int(count as i32)` (silent 64→32 bit truncation). The JDK streams
            // through a bounded buffer in a loop. Clamp the per-iteration chunk to a
            // sane ceiling and loop until `count` is satisfied or the source is
            // exhausted, so the allocation is bounded and the limit never truncates.
            const FC_XFER_CHUNK: i64 = 8 * 1024 * 1024;
            // Pin across the per-chunk allocs / read callbacks below — a
            // moving young GC there would relocate `src` (native stale-local
            // family).
            let src_pin = ctx.pin_native_root(src);
            let mut remaining = count;
            let mut cur_pos = position.max(0);
            let mut total_written: i64 = 0;
            while remaining > 0 {
                let chunk = remaining.min(FC_XFER_CHUNK) as usize;
                // Allocate a bounded ByteBuffer and call src.read(ByteBuffer)
                let byte_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, chunk);
                let byte_arr_pin = ctx.pin_native_root(byte_arr);
                let bb = alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 3);
                let bb_pin = ctx.pin_native_root(bb);
                let byte_arr = ctx.read_native_pin(byte_arr_pin, byte_arr);
                ctx.set_field(bb, 0, Value::Object(Some(byte_arr)));
                ctx.set_field(bb, 1, Value::Int(0));
                // `chunk` <= FC_XFER_CHUNK so this Int cast never truncates.
                ctx.set_field(bb, 2, Value::Int(chunk as i32));
                let src_cur = ctx.read_native_pin(src_pin, src);
                let read_n = match ctx.invoke_virtual(
                    src_cur,
                    "read",
                    "(Ljava/nio/ByteBuffer;)I",
                    &[Value::Object(Some(bb))],
                ) {
                    Ok(Some(Value::Int(n))) if n > 0 => n as usize,
                    _ => 0,
                };
                let bb = ctx.read_native_pin(bb_pin, bb);
                ctx.unpin_native_roots(byte_arr_pin);
                if read_n == 0 {
                    break;
                }
                // Extract bytes from ByteBuffer (position is now read_n)
                let mut data = vec![0u8; read_n];
                if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                    for i in 0..read_n {
                        data[i] = ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8;
                    }
                }
                let written = ctx
                    .fd_table()
                    .pwrite_at(fd_id as u32, &data, cur_pos as u64)
                    .unwrap_or(0);
                total_written += written as i64;
                cur_pos += written as i64;
                remaining -= read_n as i64;
                // Short read from the source means EOF: stop, returning bytes so far.
                if read_n < chunk {
                    break;
                }
            }
            ctx.unpin_native_roots(src_pin);
            Ok(Some(Value::Long(total_written)))
        },
    );

    // force(boolean metadata)V
    r.register(fc, "force", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        // FIX (finding 2): honor durability instead of being a silent no-op.
        // `clone_file` flushes any buffered writer for the fd and hands back a
        // std::fs::File referring to the same kernel file; sync_data/sync_all then
        // issue a real fsync (POSIX fsync / Windows FlushFileBuffers under the hood).
        // `metadata == true` → flush file contents AND metadata (sync_all); false →
        // at least the file contents (sync_data). We only fall back to best-effort
        // (no error surfaced) when the fd genuinely isn't a real file (e.g. a pipe).
        if fd_id >= 0 {
            let metadata = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
            if let Ok(file) = ctx.fd_table().clone_file(fd_id as u32) {
                let res = if metadata {
                    file.sync_all()
                } else {
                    file.sync_data()
                };
                if let Err(e) = res {
                    return Err(RuntimeError::IOException {
                        message: format!("FileChannel.force failed: {e}"),
                    }
                    .into());
                }
            }
            // Non-file-backed fd (pipe/socket/etc.): nothing to sync — best effort.
        }
        Ok(None)
    });

    // close()V
    r.register(fc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // See docs/known-issues/h2/!bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md:
        // this native is registered on the literal "java/nio/channels/FileChannel"
        // class to service a synthetic single-field FileChannel, but native
        // overrides shadow ALL dispatch for that class name -- including a real
        // `sun/nio/ch/FileChannelImpl` reaching an inherited method (close()V is
        // declared in the grandparent AbstractInterruptibleChannel, not
        // FileChannel itself) through a FileChannel-typed call site. Detect a
        // real instance and replicate AbstractInterruptibleChannel.close()'s
        // contract by calling the real implCloseChannel() bytecode instead of
        // treating field 0 as a synthetic fd.
        let class_name = ctx.class_name_of_id(ctx.class_id_of_object(this));
        if class_name.as_deref() != Some("java/nio/channels/FileChannel") {
            if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
                return Ok(None);
            }
            ctx.set_field_by_name(this, "closed", Value::Int(1));
            ctx.invoke_virtual(this, "implCloseChannel", "()V", &[])?;
            return Ok(None);
        }
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        if fd_id >= 0 {
            let _ = ctx.fd_table().close(fd_id as u32);
            ctx.set_field(this, 0, Value::Int(-1));
        }
        Ok(None)
    });

    // isOpen()Z
    r.register(fc, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let class_name = ctx.class_name_of_id(ctx.class_id_of_object(this));
        if class_name.as_deref() != Some("java/nio/channels/FileChannel") {
            return Ok(Some(Value::Int(
                if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
                    0
                } else {
                    1
                },
            )));
        }
        let fd_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
        Ok(Some(Value::Int(if fd_id >= 0 { 1 } else { 0 })))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// java.nio.file.attribute — BasicFileAttributes, FileTime
// BasicFileAttributes = 5-field (creation=0, lastAccess=1, lastMod=2, isDir=3, size=4)
// FileTime = 1-field (millis=0 Long)
// =============================================================================

pub(crate) fn register_p59_file_attributes(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // FileTreeWalker hands `Files.find` the concrete platform attributes
    // implementation, and its predicate invokes `isRegularFile` virtually on
    // that class.  Registering only on the BasicFileAttributes interface is
    // insufficient when the interpreter resolves the concrete override first:
    // every file then appeared non-regular and Files.find produced an empty
    // stream for a mounted embedded JAR.
    for attrs_class in [
        "java/nio/file/attribute/BasicFileAttributes",
        "sun/nio/fs/WindowsFileAttributes",
        "sun/nio/fs/UnixFileAttributes",
    ] {
        r.register(
            attrs_class,
            "creationTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let millis = basic_file_attributes_time_millis(ctx, this, "creation");
                Ok(Some(Value::Object(Some(filetime_alloc(ctx, millis)))))
            },
        );
        r.register(
            attrs_class,
            "lastAccessTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let millis = basic_file_attributes_time_millis(ctx, this, "access");
                Ok(Some(Value::Object(Some(filetime_alloc(ctx, millis)))))
            },
        );
        r.register(
            attrs_class,
            "lastModifiedTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let millis = basic_file_attributes_time_millis(ctx, this, "modified");
                Ok(Some(Value::Object(Some(filetime_alloc(ctx, millis)))))
            },
        );
        r.register(attrs_class, "isDirectory", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(basic_file_attributes_is_dir(
                ctx, this,
            )))))
        });
        // Was `!isDirectory()`, which called a symlink read under
        // NOFOLLOW_LINKS a regular file. Check the real type bits instead.
        r.register(attrs_class, "isRegularFile", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(
                basic_file_attributes_is_regular(ctx, this),
            ))))
        });
        // Was a hardcoded `false`, so `Files.readAttributes(p, …,
        // NOFOLLOW_LINKS).isSymbolicLink()` could never be true no matter what
        // was on disk — a caller walking a tree had no way to spot a link and
        // would follow it. Read the same `st_mode`/`fileAttrs` bits the other
        // predicates use; `p59_files_read_attributes` now populates them.
        r.register(attrs_class, "isSymbolicLink", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(
                basic_file_attributes_is_symlink(ctx, this),
            ))))
        });
        // Was a hardcoded `false` on the claim that nothing this VM's Path
        // surface can name is a device/socket/FIFO — but a `Path` is just a
        // string, and `Files.readAttributes` on `/dev/*`, `/proc/*` or a unix
        // socket lands here. The real bodies (`UnixFileAttributes.isOther()`,
        // `WindowsFileAttributes.isOther()`) are literally
        // `!isRegularFile() && !isDirectory() && !isSymbolicLink()`, and all
        // three of those already read the real `st_mode` / `fileAttrs` bits
        // that `p59_files_read_attributes` populates. Compose them.
        r.register(attrs_class, "isOther", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = !basic_file_attributes_is_regular(ctx, this)
                && !basic_file_attributes_is_dir(ctx, this)
                && !basic_file_attributes_is_symlink(ctx, this);
            Ok(Some(Value::Int(i32::from(other))))
        });
        r.register(attrs_class, "size", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Long(basic_file_attributes_size(ctx, this))))
        });
        // Was an unconditional `null` — legal on paper (the JDK returns null on
        // FAT-class volumes / network shares) but it meant NO file this VM ever
        // described had an identity, so `FileTreeWalker.wouldLoop` could never
        // detect a symlink cycle and `Files.isSameFile`-style checks had nothing
        // to compare. `p59_files_read_attributes` now records the real OS
        // identity; see `basic_file_attributes_file_key`.
        r.register(
            attrs_class,
            "fileKey",
            "()Ljava/lang/Object;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                match basic_file_attributes_file_key(ctx, this) {
                    Some(key) => {
                        let s = ctx.create_string(&key);
                        Ok(Some(Value::Object(Some(s))))
                    }
                    None => Ok(Some(Value::Object(None))),
                }
            },
        );
    }

    // FileTime — see `filetime_alloc` / `filetime_read_millis`. The millis is
    // stored in the real `long value` field (by name) so descriptor coercion
    // doesn't destroy it; slot 0 of a real-JDK-bound FileTime is a *reference*
    // field (`instant`/`unit` cache), so a raw `set_field(ft, 0, Long)` was
    // coerced to null — `toMillis()` returned 0 (vs the real mtime on HotSpot).
    let ft = "java/nio/file/attribute/FileTime";
    r.register(ft, "toMillis", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(filetime_read_millis(ctx, this))))
    });
    r.register(
        ft,
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            let millis = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let obj = filetime_alloc(ctx, millis);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ft,
        "compareTo",
        "(Ljava/nio/file/attribute/FileTime;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let a = filetime_read_millis(ctx, this);
            let b = filetime_read_millis(ctx, other);
            Ok(Some(Value::Int(a.cmp(&b) as i32)))
        },
    );
    r.register(ft, "toString", "()Ljava/lang/String;", |ctx, args| {
        // Real FileTime.toString is the ISO-8601 instant (was "{millis}ms").
        let this = obj_arg(args, 0)?;
        let millis = filetime_read_millis(ctx, this);
        let inst = match ctx.invoke(
            "java/time/Instant",
            "ofEpochMilli",
            "(J)Ljava/time/Instant;",
            &[Value::Long(millis)],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                let s = ctx.create_string("");
                return Ok(Some(Value::Object(Some(s))));
            }
        };
        ctx.invoke_virtual(inst, "toString", "()Ljava/lang/String;", &[])
    });

    // The native bridge supplies the concrete attributes object directly, so
    // it must preserve the provider's requested-type contract itself. In
    // particular, Windows cannot manufacture a `PosixFileAttributes` object:
    // returning `WindowsFileAttributes` makes generic callers observe a
    // successful read (or a later bad cast) instead of the JDK's documented
    // `UnsupportedOperationException` fallback.
    r.register(
        "java/nio/file/Files",
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;",
        |ctx, args| {
            #[cfg(windows)]
            {
                // Only a PRESENT `Class` argument states a requested type. A
                // null/absent one carries no request at all, and rejecting it
                // produced the nonsense `File attribute type  is not supported
                // on Windows` (note the empty name) for a caller that never
                // asked for anything unsupported. Fall back to the method's
                // declared return type, `BasicFileAttributes`, which Windows
                // does support.
                if let Some(Value::Object(Some(class))) = args.get(1) {
                    let requested_type =
                        crate::lang_class::mirror_class_name(ctx, *class).unwrap_or_default();
                    if !requested_type.is_empty()
                        && !windows_supports_file_attributes_type(&requested_type)
                    {
                        return Err(RuntimeError::UnsupportedOperationException {
                            message: format!(
                                "File attribute type {requested_type} is not supported on Windows"
                            ),
                        }
                        .into());
                    }
                }
            }
            let path = args.first().copied().unwrap_or(Value::Object(None));
            let options = args.get(2).copied().unwrap_or(Value::Object(None));
            p59_files_read_attributes(ctx, &[path, options])
        },
    );
    r.register(
        "java/nio/file/Files",
        "getLastModifiedTime",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            let path_str = extract_path_string(ctx, args.first());
            // NIO contract: a missing file must raise `NoSuchFileException`
            // (see `newByteChannel` above), not silently answer with a
            // zero/epoch `FileTime` — callers like
            // `FileSystemResource.lastModified()` explicitly catch
            // `NoSuchFileException` and translate it to
            // `FileNotFoundException` (ResourceTests#resourceCreateRelativeUnknown).
            let meta = match std::fs::metadata(&path_str) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(p57_no_such_file(ctx, &path_str));
                }
                Err(e) => return Err(p57_io_error(&e)),
            };
            let millis = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let ft = filetime_alloc(ctx, millis);
            Ok(Some(Value::Object(Some(ft))))
        },
    );
    r.set_category(__prev_cat);
}

/// Allocate a `java.nio.file.attribute.FileTime` carrying `millis`.
///
/// In real-JDK mode the object binds to the real `FileTime` class (so a
/// `checkcast FileTime` / `instanceof FileTime` in `BasicFileAttributes`
/// consumers holds). The millis is written into the real `long value` field
/// **by name** so descriptor coercion keeps it a `Long`. A raw slot-0 write is
/// only used as the synthetic-jdk-mode fallback (no named `value` field):
/// slot 0 of a real FileTime is a *reference* field, so a `Long` written there
/// is coerced to null — the overlay-on-real-class bug that made `toMillis()`
/// return 0 instead of the file's mtime.
pub(crate) fn filetime_alloc(ctx: &mut dyn NativeContext, millis: i64) -> ObjectRef {
    let ft = alloc_concurrent_synthetic(ctx, "java/nio/file/attribute/FileTime", 1);
    ctx.set_field_by_name(ft, "value", Value::Long(millis));
    // Set the real `unit` field to TimeUnit.MILLISECONDS. The raw-alloc path
    // skips FileTime's constructor, so `unit` and the cached `instant` are both
    // null. Real FileTime bytecode that has no native override — notably
    // `to(TimeUnit)` (commons-compress's `FileTimes.toUnixTime` → tar entry
    // mtime) — branches on `unit`: when null it dereferences the (null)
    // `instant`, NPEing on `instant.getEpochSecond()`. With `unit` set, all the
    // real value+unit math works (to/toMillis/toInstant/compareTo).
    if let Ok(tu_cid) = ctx.ensure_class_initialized("java/util/concurrent/TimeUnit") {
        if let Some(idx) = ctx.static_field_index_by_name(tu_cid, "MILLISECONDS") {
            let ms = ctx.get_static_field(tu_cid, idx);
            if matches!(ms, Value::Object(Some(_))) {
                ctx.set_field_by_name(ft, "unit", ms);
            }
        }
    }
    // Only fall back to slot 0 when the real `value` field is absent
    // (synthetic-jdk stub) — touching slot 0 of a real FileTime would clobber
    // a reference cache field.
    if !matches!(ctx.get_field_by_name(ft, "value"), Value::Long(_)) {
        ctx.set_field(ft, 0, Value::Long(millis));
    }
    ft
}

/// Read the millis from a FileTime built by [`filetime_alloc`]: prefer the
/// real `long value` field (real-JDK mode), else slot 0 (synthetic mode).
pub(crate) fn filetime_read_millis(ctx: &dyn NativeContext, ft: ObjectRef) -> i64 {
    if let Value::Long(v) = ctx.get_field_by_name(ft, "value") {
        return v;
    }
    match ctx.get_field(ft, 0) {
        Value::Long(v) => v,
        _ => 0,
    }
}

/// Apply the non-null fields passed to `BasicFileAttributeView.setTimes`.
/// `filetime` provides portable access/modified-time updates; Windows also
/// exposes a mutable creation time, which is handled with `SetFileTime`.
pub(crate) fn set_file_attribute_times(
    path: &str,
    creation_millis: Option<i64>,
    access_millis: Option<i64>,
    modified_millis: Option<i64>,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        return set_file_attribute_times_windows(
            path,
            creation_millis,
            access_millis,
            modified_millis,
        );
    }

    #[cfg(not(windows))]
    {
        let _ = creation_millis;
        if let Some(access_millis) = access_millis {
            filetime::set_file_atime(path, filetime_from_millis(access_millis))?;
        }
        if let Some(modified_millis) = modified_millis {
            filetime::set_file_mtime(path, filetime_from_millis(modified_millis))?;
        }
        Ok(())
    }
}

pub(crate) fn filetime_from_millis(millis: i64) -> filetime::FileTime {
    let seconds = millis.div_euclid(1_000);
    let nanos = (millis.rem_euclid(1_000) * 1_000_000) as u32;
    filetime::FileTime::from_unix_time(seconds, nanos)
}

#[cfg(windows)]
pub(crate) fn set_file_attribute_times_windows(
    path: &str,
    creation_millis: Option<i64>,
    access_millis: Option<i64>,
    modified_millis: Option<i64>,
) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    unsafe extern "system" {
        fn SetFileTime(
            file: *mut std::ffi::c_void,
            creation: *const FileTime,
            access: *const FileTime,
            modified: *const FileTime,
        ) -> i32;
    }

    fn as_filetime(millis: i64) -> FileTime {
        let ticks = (i128::from(millis) + 11_644_473_600_000i128) * 10_000i128;
        let ticks = ticks as u64;
        FileTime {
            low: ticks as u32,
            high: (ticks >> 32) as u32,
        }
    }

    let creation = creation_millis.map(as_filetime);
    let access = access_millis.map(as_filetime);
    let modified = modified_millis.map(as_filetime);
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(0x0200_0000)
        .open(path)?;
    let result = unsafe {
        SetFileTime(
            file.as_raw_handle().cast(),
            creation.as_ref().map_or(ptr::null(), |time| time),
            access.as_ref().map_or(ptr::null(), |time| time),
            modified.as_ref().map_or(ptr::null(), |time| time),
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Allocate a real platform `BasicFileAttributes` implementation.
///
/// Callers perform a JVM `checkcast BasicFileAttributes`, so the carrier must
/// actually implement that interface. The native bridge stores its canonical
/// values in named platform fields and force-dispatches the interface methods;
/// it never relies on the implementation's bytecode layout.
pub(crate) fn basic_file_attributes_alloc(ctx: &mut dyn NativeContext) -> ObjectRef {
    let class_name = if cfg!(windows) {
        "sun/nio/fs/WindowsFileAttributes"
    } else {
        "sun/nio/fs/UnixFileAttributes"
    };
    if let Ok(class_id) = ctx.ensure_class_initialized(class_name) {
        let fields = ctx.class_num_total_fields(class_id);
        if ctx.class_name_of_id(class_id).as_deref() == Some(class_name) && fields > 0 {
            return ctx.alloc_object(class_id, fields);
        }
    }
    // Only reached by a synthetic-JDK configuration, where the interface is
    // modelled as a concrete helper with a five-field layout.
    alloc_concurrent_synthetic(ctx, "java/nio/file/attribute/BasicFileAttributes", 5)
}

pub(crate) fn basic_file_attributes_is_windows(ctx: &dyn NativeContext, attrs: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(attrs))
        .as_deref()
        == Some("sun/nio/fs/WindowsFileAttributes")
}

/// True when `attrs` is the synthetic-JDK stub minted by
/// `basic_file_attributes_alloc`'s fallback rather than a real
/// `sun.nio.fs.{Unix,Windows}FileAttributes`.
///
/// `java.nio.file.attribute.BasicFileAttributes` is an INTERFACE in every real
/// JDK, so no genuine instance can ever carry that class id — a hit here is
/// unambiguously our own stub.
///
/// Why this matters: `ensure_synthetic_class` declares a slot COUNT but ZERO
/// named fields, so `set_field_by_name` silently drops the write and
/// `get_field_by_name` answers `Object(None)`. Every attribute
/// `basic_file_attributes_store` wrote by name vanished, and every predicate
/// below read back its default — a plain file reported `isRegularFile() ==
/// false`, `size() == 0` and epoch timestamps. The accessors therefore switch
/// to the fixed slot layout below for this class.
pub(crate) fn basic_file_attributes_is_synthetic(
    ctx: &dyn NativeContext,
    attrs: ObjectRef,
) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(attrs))
        .as_deref()
        == Some("java/nio/file/attribute/BasicFileAttributes")
}

/// Slot layout of the synthetic `BasicFileAttributes` stub (5 slots — keep in
/// sync with the `alloc_concurrent_synthetic(..., 5)` call in
/// `basic_file_attributes_alloc`). The mode word uses the Unix `st_mode`
/// encoding (type bits | permission bits) on every host so the shared
/// predicates can decode it uniformly.
pub(crate) const BFA_SYN_SLOT_MODE: usize = 0;
pub(crate) const BFA_SYN_SLOT_SIZE: usize = 1;
pub(crate) const BFA_SYN_SLOT_CREATION: usize = 2;
pub(crate) const BFA_SYN_SLOT_ACCESS: usize = 3;
pub(crate) const BFA_SYN_SLOT_MODIFIED: usize = 4;

/// Read the synthetic stub's `st_mode`-encoded mode word.
fn basic_file_attributes_syn_mode(ctx: &dyn NativeContext, attrs: ObjectRef) -> i32 {
    match ctx.get_field(attrs, BFA_SYN_SLOT_MODE) {
        Value::Int(v) => v,
        Value::Long(v) => v as i32,
        _ => 0,
    }
}

/// The real `sun.nio.fs.UnixFileAttributes` stores every timestamp as a
/// SECONDS + NANOS *pair* (`st_mtime_sec`/`st_mtime_nsec`, `st_atime_sec`/
/// `st_atime_nsec`, `st_birthtime_sec`/`st_birthtime_nsec`). It has never had
/// an unsplit `st_mtime`/`st_atime`/`st_birthtime` field on any JDK this VM
/// targets. Returns `(seconds field, nanos field, legacy millis field)`; the
/// third is only used by a synthetic `UnixFileAttributes` shim that declares
/// the unsplit spelling.
///
/// Why this needs a helper rather than a literal: `set_field_by_name` on a
/// name the class does not declare is a SILENT no-op, and `get_field_by_name`
/// answers a non-`Long`. Storing and loading through the same wrong name made
/// this bridge perfectly self-consistent while agreeing with nothing — every
/// `BasicFileAttributeView.readAttributes()` on a Unix host reported
/// 1970-01-01 for all three times regardless of the file's real timestamps,
/// even though `setTimes` had written them to the inode correctly. The
/// Windows carrier's names (`creationTime`/`lastAccessTime`/`lastWriteTime`)
/// happen to be genuine, which is why this only ever showed up on Linux.
/// See fixed-suite-bugs/springboot/jarmode-tools-extract-timestamp-preservation-FIXED.md.
fn unix_attr_time_fields(which: &str) -> (&'static str, &'static str, &'static str) {
    match which {
        "creation" => ("st_birthtime_sec", "st_birthtime_nsec", "st_birthtime"),
        "access" => ("st_atime_sec", "st_atime_nsec", "st_atime"),
        "ctime" => ("st_ctime_sec", "st_ctime_nsec", "st_ctime"),
        _ => ("st_mtime_sec", "st_mtime_nsec", "st_mtime"),
    }
}

fn attrs_declares_field(ctx: &dyn NativeContext, attrs: ObjectRef, field: &str) -> bool {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(attrs), field)
        .is_some()
}

fn attrs_long_field(ctx: &dyn NativeContext, attrs: ObjectRef, field: &str) -> i64 {
    match ctx.get_field_by_name(attrs, field) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    }
}

pub(crate) fn basic_file_attributes_time_millis(
    ctx: &dyn NativeContext,
    attrs: ObjectRef,
    which: &str,
) -> i64 {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        let slot = match which {
            "creation" => BFA_SYN_SLOT_CREATION,
            "access" => BFA_SYN_SLOT_ACCESS,
            _ => BFA_SYN_SLOT_MODIFIED,
        };
        return match ctx.get_field(attrs, slot) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        let field = match which {
            "creation" => "creationTime",
            "access" => "lastAccessTime",
            _ => "lastWriteTime",
        };
        return attrs_long_field(ctx, attrs, field);
    }
    let (sec_field, nsec_field, legacy_field) = unix_attr_time_fields(which);
    if attrs_declares_field(ctx, attrs, sec_field) {
        let seconds = attrs_long_field(ctx, attrs, sec_field);
        let nanos = attrs_long_field(ctx, attrs, nsec_field);
        return seconds
            .saturating_mul(1_000)
            .saturating_add(nanos.div_euclid(1_000_000));
    }
    attrs_long_field(ctx, attrs, legacy_field)
}

/// Inverse of `basic_file_attributes_time_millis` for the Unix carrier.
fn unix_attr_store_time(
    ctx: &mut dyn NativeContext,
    attrs: ObjectRef,
    which: &str,
    millis: i64,
) {
    let (sec_field, nsec_field, legacy_field) = unix_attr_time_fields(which);
    if attrs_declares_field(ctx, attrs, sec_field) {
        ctx.set_field_by_name(attrs, sec_field, Value::Long(millis.div_euclid(1_000)));
        ctx.set_field_by_name(
            attrs,
            nsec_field,
            Value::Long(millis.rem_euclid(1_000) * 1_000_000),
        );
        return;
    }
    ctx.set_field_by_name(attrs, legacy_field, Value::Long(millis));
}

pub(crate) fn basic_file_attributes_is_dir(ctx: &dyn NativeContext, attrs: ObjectRef) -> bool {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        return basic_file_attributes_syn_mode(ctx, attrs) & UNIX_S_IFMT == 0o040000;
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        return matches!(ctx.get_field_by_name(attrs, "fileAttrs"), Value::Int(v) if v & 0x10 != 0);
    }
    matches!(ctx.get_field_by_name(attrs, "st_mode"), Value::Int(v) if v & 0o170000 == 0o040000)
}

/// `st_mode` type bits (`S_IFMT` / `S_IFLNK` / `S_IFREG`) and the Windows
/// `FILE_ATTRIBUTE_REPARSE_POINT` bit, used by the type predicates below.
pub(crate) const UNIX_S_IFMT: i32 = 0o170000;
pub(crate) const UNIX_S_IFLNK: i32 = 0o120000;
pub(crate) const UNIX_S_IFREG: i32 = 0o100000;
pub(crate) const WIN_ATTR_DIRECTORY: i32 = 0x10;
pub(crate) const WIN_ATTR_REPARSE_POINT: i32 = 0x400;

pub(crate) fn basic_file_attributes_is_symlink(ctx: &dyn NativeContext, attrs: ObjectRef) -> bool {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        return basic_file_attributes_syn_mode(ctx, attrs) & UNIX_S_IFMT == UNIX_S_IFLNK;
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        return matches!(ctx.get_field_by_name(attrs, "fileAttrs"),
            Value::Int(v) if v & WIN_ATTR_REPARSE_POINT != 0);
    }
    matches!(ctx.get_field_by_name(attrs, "st_mode"),
        Value::Int(v) if v & UNIX_S_IFMT == UNIX_S_IFLNK)
}

pub(crate) fn basic_file_attributes_is_regular(ctx: &dyn NativeContext, attrs: ObjectRef) -> bool {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        return basic_file_attributes_syn_mode(ctx, attrs) & UNIX_S_IFMT == UNIX_S_IFREG;
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        return matches!(ctx.get_field_by_name(attrs, "fileAttrs"),
            Value::Int(v) if v & (WIN_ATTR_DIRECTORY | WIN_ATTR_REPARSE_POINT) == 0);
    }
    matches!(ctx.get_field_by_name(attrs, "st_mode"),
        Value::Int(v) if v & UNIX_S_IFMT == UNIX_S_IFREG)
}

/// Shared body for `Files.getOwner` / `FileOwnerAttributeView.getOwner`.
///
/// Reads the path's attributes and asks the resulting attributes object for its
/// `owner()`. On a real-JDK Unix build that lands in
/// `UnixFileAttributes.owner()` -> `UnixUserPrincipals.fromUid(st_uid)`, both of
/// which are already wired (`p59_files_read_attributes` fills `st_uid`;
/// `native-io` registers `UnixNativeDispatcher.getpwuid`). Anywhere the owner
/// genuinely cannot be produced we raise the exception the JDK specifies for a
/// provider without a `FileOwnerAttributeView` — returning `null`, which is what
/// this used to do, is not a legal outcome of `getOwner` and only moves the
/// failure to the caller's next dereference.
fn nio_owner_principal(ctx: &mut dyn NativeContext, path_value: Value) -> MethodCallResult {
    let unsupported = || -> MethodCallFailed {
        RuntimeError::UnsupportedOperationException {
            message: "getOwner: file owner is not available for this path".into(),
        }
        .into()
    };
    // Windows first: `p59_files_read_attributes` produces the DOS/Windows
    // attribute shape there, which has no `owner()` at all, so the Unix
    // delegation below could only ever throw — `Files.getOwner` was
    // unconditionally `UnsupportedOperationException` on Windows while HotSpot
    // answers. `win_file_owner_account` asks the OS for the real owner SID and
    // resolves it to `DOMAIN\account`.
    #[cfg(windows)]
    {
        let path_text = match path_value {
            Value::Object(Some(p)) => p57_read_path(ctx, p),
            _ => String::new(),
        };
        if !path_text.is_empty() {
            if std::fs::symlink_metadata(&path_text).is_err() {
                return Err(p57_no_such_file(ctx, &path_text));
            }
            let Some((account, sid_text, sid_type)) = win_file_owner_account(&path_text) else {
                return Err(RuntimeError::IOException {
                    message: format!("getOwner: cannot read the owner of {path_text}"),
                }
                .into());
            };
            return Ok(Some(Value::Object(Some(alloc_windows_user_principal(
                ctx, &account, &sid_text, sid_type,
            )))));
        }
    }
    let attrs = match p59_files_read_attributes(ctx, &[path_value])? {
        Some(Value::Object(Some(attrs))) => attrs,
        _ => return Err(unsupported()),
    };
    match ctx.invoke_virtual(
        attrs,
        "owner",
        "()Ljava/nio/file/attribute/UserPrincipal;",
        &[],
    ) {
        Ok(Some(owner @ Value::Object(Some(_)))) => Ok(Some(owner)),
        _ => Err(unsupported()),
    }
}

/// The real-JDK `Files.getOwner` path reaches provider bytecode that is not
/// presently complete in CratonVM.  Reuse the attribute-backed owner bridge,
/// but do not promote the surrounding synthetic Files registrar.
pub fn register_real_jdk_files_owner(r: &mut NativeMethodRegistry) {
    r.register(
        "java/nio/file/Files",
        "getOwner",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/UserPrincipal;",
        |ctx, args| {
            let path_value = args.first().copied().unwrap_or(Value::Object(None));
            nio_owner_principal(ctx, path_value)
        },
    );
}

/// The owner of a file as Windows reports it: `(account, sid, sid_type)`.
///
/// `GetNamedSecurityInfoW(.., OWNER_SECURITY_INFORMATION, ..)` yields the owner
/// SID; `LookupAccountSidW` turns it into `DOMAIN\account` (the rendering
/// `sun.nio.fs.WindowsUserPrincipals` uses), and `ConvertSidToStringSidW` gives
/// the `S-1-5-…` form the JDK keeps for `equals`/`hashCode`. A SID with no
/// resolvable account (an orphaned ACL entry) still gets a principal, named by
/// its SID string — which is exactly what the JDK does in that case.
///
/// `None` means the OS refused the query; the caller raises `IOException`, the
/// outcome `Files.getOwner` specifies for a failed lookup.
#[cfg(windows)]
fn win_file_owner_account(path: &str) -> Option<(String, String, i32)> {
    const SE_FILE_OBJECT: i32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
    const ERROR_SUCCESS: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const ERROR_NONE_MAPPED: u32 = 1332;

    #[link(name = "Advapi32")]
    extern "system" {
        fn GetNamedSecurityInfoW(
            object_name: *const u16,
            object_type: i32,
            security_info: u32,
            owner: *mut *mut std::ffi::c_void,
            group: *mut *mut std::ffi::c_void,
            dacl: *mut *mut std::ffi::c_void,
            sacl: *mut *mut std::ffi::c_void,
            security_descriptor: *mut *mut std::ffi::c_void,
        ) -> u32;
        fn LookupAccountSidW(
            system_name: *const u16,
            sid: *mut std::ffi::c_void,
            name: *mut u16,
            name_len: *mut u32,
            domain: *mut u16,
            domain_len: *mut u32,
            use_: *mut i32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut std::ffi::c_void, out: *mut *mut u16) -> i32;
    }
    #[link(name = "Kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn GetLastError() -> u32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    let wpath = wide(path);
    let mut owner: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut descriptor: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `wpath` is NUL-terminated and outlives the call; every out-pointer
    // is a live local. On success the descriptor is a single LocalAlloc block
    // that owns `owner`, freed exactly once below and never escaping.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if rc != ERROR_SUCCESS || owner.is_null() {
        if !descriptor.is_null() {
            // SAFETY: allocated by the call above, freed once.
            unsafe { LocalFree(descriptor) };
        }
        return None;
    }

    // SID string form (`S-1-5-21-…`), used as the principal's identity.
    let mut sid_text = String::new();
    let mut sid_wide: *mut u16 = std::ptr::null_mut();
    // SAFETY: `owner` points into the live descriptor; the returned buffer is a
    // LocalAlloc block freed immediately after it is copied out.
    if unsafe { ConvertSidToStringSidW(owner, &mut sid_wide) } != 0 && !sid_wide.is_null() {
        let mut len = 0usize;
        // SAFETY: the OS returns a NUL-terminated UTF-16 string.
        while unsafe { *sid_wide.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` stops at the NUL, so the slice is in bounds.
        sid_text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_wide, len) });
        // SAFETY: allocated by ConvertSidToStringSidW, freed once.
        unsafe { LocalFree(sid_wide.cast()) };
    }

    // Account name. Two-call idiom: ask for the required sizes, then fill.
    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut sid_type: i32 = 0;
    // SAFETY: zero lengths with null buffers is the documented sizing call; it
    // fails with ERROR_INSUFFICIENT_BUFFER and writes the two lengths.
    unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            owner,
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut sid_type,
        )
    };
    let sizing_error = unsafe { GetLastError() };
    let account = if sizing_error == ERROR_INSUFFICIENT_BUFFER && name_len > 0 {
        let mut name = vec![0u16; name_len as usize];
        let mut domain = vec![0u16; domain_len.max(1) as usize];
        // SAFETY: both buffers are sized by the call above and outlive this one.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                owner,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut sid_type,
            )
        };
        if ok != 0 {
            let account = from_wide(&name);
            let domain = from_wide(&domain);
            if domain.is_empty() {
                account
            } else {
                format!("{domain}\\{account}")
            }
        } else {
            String::new()
        }
    } else if sizing_error == ERROR_NONE_MAPPED {
        // A SID with no account behind it. The JDK names the principal by its
        // SID string rather than failing, so do the same.
        String::new()
    } else {
        String::new()
    };

    // SAFETY: `descriptor` came from the successful call above and is freed once;
    // `owner` points inside it and is not used after this.
    unsafe { LocalFree(descriptor) };

    if account.is_empty() && sid_text.is_empty() {
        return None;
    }
    let account = if account.is_empty() {
        sid_text.clone()
    } else {
        account
    };
    Some((account, sid_text, sid_type))
}

/// Materialise the `UserPrincipal` `Files.getOwner` hands back on Windows.
///
/// Uses the real JDK carrier `sun.nio.fs.WindowsUserPrincipals$User` and writes
/// its three fields BY NAME, so the object satisfies `instanceof UserPrincipal`
/// and any JDK code that reads those fields directly sees the right values. The
/// accessors below are registered on the same class, so a build where that class
/// is synthesized rather than loaded still answers.
#[cfg(windows)]
fn alloc_windows_user_principal(
    ctx: &mut dyn NativeContext,
    account: &str,
    sid_text: &str,
    sid_type: i32,
) -> cratonvm_types::ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "sun/nio/fs/WindowsUserPrincipals$User", 3);
    // GC-SAFETY: `create_string` allocates, so pin the carrier and re-read it
    // through the pin after each allocation before writing into it.
    let pin = ctx.pin_native_root(obj);
    let sid_str = ctx.create_string(sid_text);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "sidString", Value::Object(Some(sid_str)));
    let account_str = ctx.create_string(account);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "accountName", Value::Object(Some(account_str)));
    ctx.set_field_by_name(obj, "sidType", Value::Int(sid_type));
    ctx.unpin_native_roots(pin);
    obj
}

/// Same identity key as `basic_file_attributes_file_key`, but computed straight
/// from a host path — for attribute objects that record their backing path
/// instead of the raw `stat` fields (`DosFileAttributes`, slot 5).
pub(crate) fn file_identity_key_for_path(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(format!(
            "(dev={},ino={})",
            meta.dev() as i64,
            meta.ino() as i64
        ))
    }
    #[cfg(windows)]
    {
        let (volume, high, low) = win_file_identity(path)?;
        let index = (u64::from(high) << 32) | u64::from(low);
        Some(format!("(volume={volume},file={index})"))
    }
}

/// The file-identity string behind `BasicFileAttributes.fileKey()`.
///
/// `fileKey()` used to be a hardcoded `null`, which silently disabled every
/// identity check built on it — most importantly `FileTreeWalker.wouldLoop`,
/// the JDK walker's ONLY symlink-loop guard (`Files.walk(.., FOLLOW_LINKS)`
/// over a directory that links back to an ancestor recursed until it ran out
/// of depth), plus hardlink identity.
///
/// `p59_files_read_attributes` now records the OS identity on the attributes
/// object (`st_dev`/`st_ino` on Unix, the `GetFileInformationByHandle` triple
/// on Windows), so the key is a real answer rather than a fabricated one. We
/// hand back a `String` in the JDK's own `UnixFileKey.toString()` shape: the
/// declared return type is `Object`, and every consumer only ever
/// `equals`/`hashCode`-compares the value, both of which `String` gives us for
/// free (the JDK's own key classes are package-private in `sun.nio.fs`, so no
/// caller can name the concrete type). `None` -> `null`, which stays the
/// documented answer when the identity is unavailable.
pub(crate) fn basic_file_attributes_file_key(
    ctx: &dyn NativeContext,
    attrs: ObjectRef,
) -> Option<String> {
    if basic_file_attributes_is_windows(ctx, attrs) {
        let field = |name: &str| match ctx.get_field_by_name(attrs, name) {
            Value::Int(v) => Some(v as u32),
            Value::Long(v) => Some(v as u32),
            _ => None,
        };
        let volume = field("volSerialNumber")?;
        let high = field("fileIndexHigh")?;
        let low = field("fileIndexLow")?;
        if volume == 0 && high == 0 && low == 0 {
            return None;
        }
        let index = (u64::from(high) << 32) | u64::from(low);
        return Some(format!("(volume={volume},file={index})"));
    }
    let field = |name: &str| match ctx.get_field_by_name(attrs, name) {
        Value::Long(v) => Some(v),
        Value::Int(v) => Some(i64::from(v)),
        _ => None,
    };
    let dev = field("st_dev")?;
    let ino = field("st_ino")?;
    if dev == 0 && ino == 0 {
        return None;
    }
    Some(format!("(dev={dev},ino={ino})"))
}

pub(crate) fn basic_file_attributes_size(ctx: &dyn NativeContext, attrs: ObjectRef) -> i64 {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        return match ctx.get_field(attrs, BFA_SYN_SLOT_SIZE) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
    }
    let field = if basic_file_attributes_is_windows(ctx, attrs) {
        "size"
    } else {
        "st_size"
    };
    match ctx.get_field_by_name(attrs, field) {
        Value::Long(v) => v,
        _ => 0,
    }
}

pub(crate) fn basic_file_attributes_store(
    ctx: &mut dyn NativeContext,
    attrs: ObjectRef,
    is_dir: bool,
    size: i64,
    creation_millis: i64,
    access_millis: i64,
    modified_millis: i64,
    unix_perm_bits: i32,
) {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        // Synthetic stub: NO named fields exist, so every `set_field_by_name`
        // below would be a silent no-op. Write the fixed slot layout the
        // accessors read (`BFA_SYN_SLOT_*`).
        let type_bits = if is_dir { 0o040000 } else { 0o100000 };
        ctx.set_field(
            attrs,
            BFA_SYN_SLOT_MODE,
            Value::Int(type_bits | (unix_perm_bits & 0o7777)),
        );
        ctx.set_field(attrs, BFA_SYN_SLOT_SIZE, Value::Long(size));
        ctx.set_field(attrs, BFA_SYN_SLOT_CREATION, Value::Long(creation_millis));
        ctx.set_field(attrs, BFA_SYN_SLOT_ACCESS, Value::Long(access_millis));
        ctx.set_field(attrs, BFA_SYN_SLOT_MODIFIED, Value::Long(modified_millis));
        return;
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        ctx.set_field_by_name(
            attrs,
            "fileAttrs",
            Value::Int(if is_dir { 0x10 } else { 0 }),
        );
        ctx.set_field_by_name(attrs, "creationTime", Value::Long(creation_millis));
        ctx.set_field_by_name(attrs, "lastAccessTime", Value::Long(access_millis));
        ctx.set_field_by_name(attrs, "lastWriteTime", Value::Long(modified_millis));
        ctx.set_field_by_name(attrs, "size", Value::Long(size));
    } else {
        // Was always the bare file-type bits with zero permission bits, so
        // `sun/nio/fs/UnixFileAttributes.permissions()` (real JDK bytecode,
        // reads this same `st_mode` field) always answered an empty
        // `Set<PosixFilePermission>` regardless of the file's real mode —
        // harmless while nothing read `permissions()`, but once
        // `PosixFileAttributeView` became reachable (see
        // `getFileAttributeView` above) this fed `FilePathDisk.setReadOnly`'s
        // "keep everything except *_WRITE" recomputation from a permission
        // set that never had bits to keep. OR in the real mode bits when the
        // caller has them (0 for the jar/jrt-FS and directory-walk callers
        // below, which have no real backing inode to query).
        let type_bits = if is_dir { 0o040000 } else { 0o100000 };
        ctx.set_field_by_name(
            attrs,
            "st_mode",
            Value::Int(type_bits | (unix_perm_bits & 0o7777)),
        );
        unix_attr_store_time(ctx, attrs, "creation", creation_millis);
        unix_attr_store_time(ctx, attrs, "access", access_millis);
        unix_attr_store_time(ctx, attrs, "modified", modified_millis);
        // `UnixFileAttributes.creationTime()` (real JDK bytecode) only trusts
        // `st_birthtime_*` when this flag is set, and falls back to `ctime()`
        // otherwise; `ctime()` reads `st_ctime_*`, which `std::fs::Metadata`
        // cannot supply portably. Mirror the modified time there so a caller
        // that reaches the real accessor instead of our override sees a
        // plausible value rather than 1970. Callers with no backing inode
        // (jar-FS / jrt-FS / directory walks) pass 0 and correctly report the
        // birth time as unavailable.
        ctx.set_field_by_name(
            attrs,
            "birthtime_available",
            Value::Int(i32::from(creation_millis != 0)),
        );
        unix_attr_store_time(ctx, attrs, "ctime", modified_millis);
        ctx.set_field_by_name(attrs, "st_size", Value::Long(size));
    }
}

/// Extract path string from a Path argument (field 0 = String)
pub(crate) fn extract_path_string(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> String {
    let raw = match arg {
        Some(Value::Object(Some(path_obj))) => match ctx.get_field(*path_obj, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => return String::new(),
        },
        _ => return String::new(),
    };
    // Match `p57_read_path`'s OS-path conversion (strips the leading `/`
    // from a `/C:/...` Windows drive path). Without it, callers of this
    // helper (e.g. `p59_files_read_attributes`) did a filesystem lookup
    // against the raw `/C:/...` string, which Windows does not resolve the
    // same way as `C:/...` — so a `Files.readAttributes`/
    // `getLastModifiedTime` NotFound check could disagree with
    // `Files.exists()` (which already goes through `p57_read_path`) for the
    // exact same logical path.
    p57_to_os_path(&raw)
}

pub(crate) fn p59_files_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let path_str = extract_path_string(ctx, args.first());
    if let Some(bytes) = vfs_read(&path_str) {
        return match bytes {
            Ok(b) => Ok(Some(Value::Long(b.len() as i64))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(p57_no_such_file(ctx, &path_str))
            }
            Err(e) => Err(p57_io_error(&e)),
        };
    }

    match std::fs::metadata(&path_str) {
        Ok(meta) => Ok(Some(Value::Long(meta.len() as i64))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(p57_no_such_file(ctx, &path_str)),
        Err(e) => Err(p57_io_error(&e)),
    }
}

/// Convert a SystemTime to epoch millis
pub(crate) fn system_time_to_millis(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Attribute interfaces backed by the Windows attributes object that this VM
/// can actually construct. Other requested interfaces must fail at the
/// provider boundary, matching the JDK instead of relying on a caller cast.
#[cfg(windows)]
fn windows_supports_file_attributes_type(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/nio/file/attribute/BasicFileAttributes" | "java/nio/file/attribute/DosFileAttributes"
    )
}

// ---------------------------------------------------------------------------
// Name-keyed attribute reads — `readAttributes(path, "view:attrs", options)`
// ---------------------------------------------------------------------------

/// Split a `"view:attrs"` attribute spec. No colon means the `basic` view,
/// exactly as `java.nio.file.Files` does it.
pub(crate) fn split_attribute_spec(spec: &str) -> (&str, &str) {
    match spec.find(':') {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => ("basic", spec),
    }
}

/// The attribute names a given view answers, in the JDK's own vocabulary.
///
/// Kept as one table rather than spread across the reader below so the `*`
/// expansion and the per-name lookup can never disagree about what a view
/// contains — a mismatch there is exactly how "`*` returned it but asking for
/// it by name threw" bugs happen.
fn attribute_names_for_view(view: &str) -> Option<&'static [&'static str]> {
    const BASIC: &[&str] = &[
        "lastModifiedTime",
        "lastAccessTime",
        "creationTime",
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
    ];
    const POSIX: &[&str] = &[
        "lastModifiedTime",
        "lastAccessTime",
        "creationTime",
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
        "permissions",
        "owner",
        "group",
    ];
    const UNIX: &[&str] = &[
        "lastModifiedTime",
        "lastAccessTime",
        "creationTime",
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
        "permissions",
        "owner",
        "group",
        "mode",
        "ino",
        "dev",
        "rdev",
        "nlink",
        "uid",
        "gid",
        "ctime",
    ];
    const DOS: &[&str] = &[
        "lastModifiedTime",
        "lastAccessTime",
        "creationTime",
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
        "readonly",
        "hidden",
        "archive",
        "system",
    ];
    const OWNER: &[&str] = &["owner"];
    match view {
        "basic" => Some(BASIC),
        "posix" => Some(POSIX),
        "unix" => Some(UNIX),
        "dos" => Some(DOS),
        "owner" => Some(OWNER),
        _ => None,
    }
}

/// Everything one `stat` tells us, in the shapes the attribute names want.
struct StatFacts {
    is_dir: bool,
    is_regular: bool,
    is_symlink: bool,
    size: i64,
    modified_millis: i64,
    access_millis: i64,
    creation_millis: i64,
    mode: i32,
    nlink: i32,
    uid: i32,
    gid: i32,
    dev: i64,
    ino: i64,
    rdev: i64,
    ctime_millis: i64,
    readonly: bool,
    hidden: bool,
}

fn stat_facts(path: &str, nofollow: bool) -> std::io::Result<StatFacts> {
    let meta = if nofollow {
        std::fs::symlink_metadata(path)?
    } else {
        std::fs::metadata(path)?
    };
    let ft = meta.file_type();
    #[allow(unused_mut)]
    let mut facts = StatFacts {
        is_dir: meta.is_dir(),
        is_regular: meta.is_file(),
        is_symlink: ft.is_symlink(),
        size: meta.len() as i64,
        modified_millis: meta.modified().ok().map(system_time_to_millis).unwrap_or(0),
        access_millis: meta.accessed().ok().map(system_time_to_millis).unwrap_or(0),
        creation_millis: meta.created().ok().map(system_time_to_millis).unwrap_or(0),
        mode: 0,
        nlink: 1,
        uid: 0,
        gid: 0,
        dev: 0,
        ino: 0,
        rdev: 0,
        ctime_millis: 0,
        readonly: meta.permissions().readonly(),
        // `isOther`/`hidden` are the two the JDK derives rather than stats.
        hidden: std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.')),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        facts.mode = meta.mode() as i32;
        facts.nlink = meta.nlink() as i32;
        facts.uid = meta.uid() as i32;
        facts.gid = meta.gid() as i32;
        facts.dev = meta.dev() as i64;
        facts.ino = meta.ino() as i64;
        facts.rdev = meta.rdev() as i64;
        facts.ctime_millis = meta
            .ctime()
            .saturating_mul(1000)
            .saturating_add(meta.ctime_nsec() / 1_000_000);
    }
    Ok(facts)
}

/// `Set<PosixFilePermission>` for a Unix mode word, built from the enum's own
/// nine singleton constants so it compares equal to anything the JDK produced.
fn posix_permission_set(ctx: &mut dyn NativeContext, mode: i32) -> Option<ObjectRef> {
    const NAMES: [&str; 9] = [
        "OWNER_READ",
        "OWNER_WRITE",
        "OWNER_EXECUTE",
        "GROUP_READ",
        "GROUP_WRITE",
        "GROUP_EXECUTE",
        "OTHERS_READ",
        "OTHERS_WRITE",
        "OTHERS_EXECUTE",
    ];
    const BITS: [i32; 9] = [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ];
    let set = match ctx.new_object_initialized("java/util/HashSet", "()V", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => return None,
    };
    let set_pin = ctx.pin_native_root(set);
    let pfp = "java/nio/file/attribute/PosixFilePermission";
    let _ = ctx.ensure_class_initialized(pfp);
    let cid = ctx.class_id_by_name(pfp);
    for i in 0..9 {
        if mode & BITS[i] == 0 {
            continue;
        }
        let Some(c) = cid else { break };
        let Some(slot) = ctx.static_field_index_by_name(c, NAMES[i]) else {
            continue;
        };
        let constant = ctx.get_static_field(c, slot);
        if !matches!(constant, Value::Object(Some(_))) {
            continue;
        }
        let set = ctx.read_native_pin(set_pin, set);
        let _ = ctx.invoke_virtual(set, "add", "(Ljava/lang/Object;)Z", &[constant]);
    }
    let set = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(set_pin);
    Some(set)
}

/// What `setAttribute` does with one attribute name.
///
/// Split out of [`write_named_attribute`] so the classification can be asserted
/// against [`attribute_names_for_view`] in a unit test: that table is what the
/// reader answers, and a name added there without a verdict here is exactly how
/// "`readAttributes` returned it but writing it says it does not exist" appears.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AttributeWrite {
    /// One of the three `FileTime` fields.
    Time,
    /// A DOS boolean flag.
    DosFlag,
    /// A POSIX permission set (`posix:permissions`).
    Permissions,
    /// A raw `unix:mode` int.
    Mode,
    /// Writable in the real JDK; not implemented here, and refused loudly.
    Unimplemented,
    /// The reader answers it, but the JDK lets nobody write it.
    NotWritable,
}

pub(crate) fn attribute_write_kind(name: &str) -> AttributeWrite {
    match name {
        "lastModifiedTime" | "lastAccessTime" | "creationTime" => AttributeWrite::Time,
        "readonly" | "hidden" | "archive" | "system" => AttributeWrite::DosFlag,
        "permissions" => AttributeWrite::Permissions,
        "mode" => AttributeWrite::Mode,
        "owner" | "group" | "uid" | "gid" => AttributeWrite::Unimplemented,
        _ => AttributeWrite::NotWritable,
    }
}

/// The write side of `Files.setAttribute(path, "view:name", value)`.
///
/// Screened against exactly the same view/name tables as
/// [`read_named_attributes`], so the two can never disagree about what a view
/// contains — asking for a name the reader answers and being told it does not
/// exist is the confusing half of that class of bug.
///
/// The writes themselves delegate: the DOS flags go through the very
/// `dos_view_set_*` natives `getFileAttributeView(path, DosFileAttributeView)`
/// hands out, and the times/permissions go through the same path-level helpers
/// those views use. Nothing about the filesystem is reimplemented here, so a
/// later fix to any of them reaches this entry point too.
///
/// Names that exist but are read-only (`size`, `isDirectory`, `fileKey`, …)
/// raise `IllegalArgumentException` with the JDK's own wording. Names that are
/// writable in the real JDK but not implemented here (`owner`, `group`, `uid`,
/// `gid`) raise `UnsupportedOperationException` rather than silently doing
/// nothing — a `setAttribute` that returns normally and changes nothing is the
/// failure mode that made Gradle believe it had marked a cache directory
/// read-only.
pub(crate) fn write_named_attribute(
    ctx: &mut dyn NativeContext,
    path_obj: ObjectRef,
    spec: &str,
    value: Option<ObjectRef>,
    nofollow: bool,
) -> MethodCallResult {
    let (view, name) = split_attribute_spec(spec);
    let Some(known) = attribute_names_for_view(view) else {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("View '{view}' not available"),
        }
        .into());
    };
    if !supported_attribute_view_names().contains(&view) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("View '{view}' not available"),
        }
        .into());
    }
    if name.is_empty() || !known.iter().any(|k| *k == name) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("'{view}:{name}' is not a recognized attribute"),
        }
        .into());
    }

    let path = p57_read_path(ctx, path_obj);

    // Every write below follows symlinks. Rather than silently write through a
    // link the caller asked us not to follow, refuse — the call sites that
    // matter never pass NOFOLLOW_LINKS, and a wrong target is worse than a
    // refusal.
    if nofollow
        && std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("setAttribute('{spec}'): NOFOLLOW_LINKS on a symbolic link"),
        }
        .into());
    }

    match attribute_write_kind(name) {
        AttributeWrite::Time => {
            let Some(ft) = value else {
                return Err(RuntimeError::NullPointerException {
                    message: Some(format!("setAttribute('{spec}'): null FileTime")),
                }
                .into());
            };
            let millis = filetime_read_millis(ctx, ft);
            let (creation, access, modified) = match name {
                "creationTime" => (Some(millis), None, None),
                "lastAccessTime" => (None, Some(millis), None),
                _ => (None, None, Some(millis)),
            };
            set_file_attribute_times(&path, creation, access, modified)
                .map_err(|error| p57_io_error(&error))?;
            Ok(None)
        }
        AttributeWrite::DosFlag => {
            let on = boolean_attribute_value(ctx, spec, value)?;
            // Build the one-field synthetic view the `dos_view_set_*` natives
            // read their path out of — the same shape `getFileAttributeView`
            // hands to Java callers. Pin the Path across the allocation: a
            // moving young GC there would relocate it out from under us.
            let path_pin = ctx.pin_native_root(path_obj);
            let view_obj =
                alloc_concurrent_synthetic(ctx, "java/nio/file/attribute/DosFileAttributeView", 1);
            let path_obj = ctx.read_native_pin(path_pin, path_obj);
            ctx.set_field(view_obj, 0, Value::Object(Some(path_obj)));
            ctx.unpin_native_roots(path_pin);
            let view_args = [
                Value::Object(Some(view_obj)),
                Value::Int(if on { 1 } else { 0 }),
            ];
            match name {
                "readonly" => dos_view_set_read_only(ctx, &view_args),
                "hidden" => dos_view_set_hidden(ctx, &view_args),
                "archive" => dos_view_set_archive(ctx, &view_args),
                _ => dos_view_set_system(ctx, &view_args),
            }
        }
        AttributeWrite::Permissions => {
            let Some(set) = value else {
                return Err(RuntimeError::NullPointerException {
                    message: Some(format!("setAttribute('{spec}'): null permission set")),
                }
                .into());
            };
            let mode = posix_permission_bits_from_set(ctx, set);
            set_file_mode(&path, mode)
        }
        AttributeWrite::Mode => {
            let mode = int_attribute_value(ctx, spec, value)?;
            set_file_mode(&path, (mode as u32) & 0o7777)
        }
        AttributeWrite::Unimplemented => Err(RuntimeError::UnsupportedOperationException {
            message: format!("setAttribute('{spec}') is not implemented"),
        }
        .into()),
        // A name the reader answers but the JDK does not let anyone write.
        AttributeWrite::NotWritable => Err(RuntimeError::IllegalArgumentException {
            message: format!("'{view}:{name}' is not a recognized attribute"),
        }
        .into()),
    }
}

/// chmod for the POSIX/unix write paths. A no-op refusal on a host without
/// POSIX modes, rather than a silent success.
fn set_file_mode(path: &str, mode: u32) -> MethodCallResult {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| p57_io_error(&error))?;
        Ok(None)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Err(RuntimeError::UnsupportedOperationException {
            message: "POSIX file modes are not supported on this host".to_string(),
        }
        .into())
    }
}

/// Unbox the `Object` value of a boolean-valued attribute. The JDK casts, so a
/// wrong type is a `ClassCastException` there and here.
fn boolean_attribute_value(
    ctx: &mut dyn NativeContext,
    spec: &str,
    value: Option<ObjectRef>,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    match value.map(|v| crate::lang_class::unbox_value(ctx, v)) {
        Some(Value::Int(i)) => Ok(i != 0),
        Some(_) | None => Err(RuntimeError::ClassCastException {
            message: format!("setAttribute('{spec}'): value is not a Boolean"),
        }
        .into()),
    }
}

/// Unbox the `Object` value of an int-valued attribute (`unix:mode`).
fn int_attribute_value(
    ctx: &mut dyn NativeContext,
    spec: &str,
    value: Option<ObjectRef>,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    match value.map(|v| crate::lang_class::unbox_value(ctx, v)) {
        Some(Value::Int(i)) => Ok(i),
        Some(Value::Long(l)) => Ok(l as i32),
        Some(_) | None => Err(RuntimeError::ClassCastException {
            message: format!("setAttribute('{spec}'): value is not an Integer"),
        }
        .into()),
    }
}

/// Read `path`'s attributes named by `spec` into a `java.util.HashMap`.
///
/// This is the whole of `Files.readAttributes(Path, String, LinkOption...)`,
/// `Files.getAttribute` and `FileSystemProvider.readAttributes(Path, String,
/// LinkOption...)`. It follows the JDK's error contract rather than degrading:
/// an unknown view is `UnsupportedOperationException`, an unknown attribute name
/// is `IllegalArgumentException`, and a missing file is `NoSuchFileException`.
/// An empty map is never a valid answer here — the previous implementation
/// returned one for every call, and every caller reads the map with `get`, so
/// the failure surfaced as a `null` attribute arbitrarily far away.
pub(crate) fn read_named_attributes(
    ctx: &mut dyn NativeContext,
    path_obj: ObjectRef,
    spec: &str,
    nofollow: bool,
) -> MethodCallResult {
    let (view, requested) = split_attribute_spec(spec);
    let Some(known) = attribute_names_for_view(view) else {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("View '{view}' not available"),
        }
        .into());
    };
    if !supported_attribute_view_names().contains(&view) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("View '{view}' not available"),
        }
        .into());
    }
    if requested.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("'{spec}' not recognized"),
        }
        .into());
    }
    let wildcard = requested == "*";
    let names: Vec<&str> = if wildcard {
        known.to_vec()
    } else {
        let mut out = Vec::new();
        for name in requested.split(',') {
            let name = name.trim();
            if !known.iter().any(|k| *k == name) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("'{view}:{name}' not recognized"),
                }
                .into());
            }
            out.push(name);
        }
        out
    };

    let path = p57_read_path(ctx, path_obj);
    let facts = match stat_facts(&path, nofollow) {
        Ok(f) => f,
        Err(_) => return Err(p57_no_such_file(ctx, &path)),
    };

    let map = match ctx.new_object_initialized("java/util/LinkedHashMap", "()V", &[]) {
        Ok(Some(Value::Object(Some(m)))) => m,
        // The class library could not give us a Map. Refuse rather than hand
        // back a shape the caller will silently read `null` out of.
        _ => {
            return Err(RuntimeError::IOException {
                message: format!("readAttributes({spec}): could not allocate the result map"),
            }
            .into())
        }
    };
    let map_pin = ctx.pin_native_root(map);

    // `owner`/`group` come from the attribute object the rest of this file
    // already builds, so principals stay one implementation.
    let mut owner_group: Option<ObjectRef> = None;
    if names.iter().any(|n| *n == "owner" || *n == "group") {
        if let Ok(Some(Value::Object(Some(attrs)))) =
            p59_files_read_attributes(ctx, &[Value::Object(Some(path_obj))])
        {
            owner_group = Some(attrs);
        }
    }
    let attrs_pin = owner_group.map(|a| ctx.pin_native_root(a));

    for name in names {
        // The key is built and pinned BEFORE the value, and nothing between the
        // value and the `put` allocates. The other order — value, then
        // `create_string(name)` — holds a fresh, unrooted attribute object
        // across an allocation, which is the native stale-local family: a young
        // collection there relocates it and the map gets a dangling entry.
        let key = ctx.create_string(name);
        let key_pin = ctx.pin_native_root(key);
        let value: Value = match name {
            "lastModifiedTime" => Value::Object(Some(filetime_alloc(ctx, facts.modified_millis))),
            "lastAccessTime" => Value::Object(Some(filetime_alloc(ctx, facts.access_millis))),
            "creationTime" => Value::Object(Some(filetime_alloc(ctx, facts.creation_millis))),
            "ctime" => Value::Object(Some(filetime_alloc(ctx, facts.ctime_millis))),
            "size" => box_long(ctx, facts.size),
            "isRegularFile" => box_boolean(ctx, facts.is_regular),
            "isDirectory" => box_boolean(ctx, facts.is_dir),
            "isSymbolicLink" => box_boolean(ctx, facts.is_symlink),
            "isOther" => box_boolean(ctx, !facts.is_regular && !facts.is_dir && !facts.is_symlink),
            // The JDK's Unix fileKey is `(dev, ino)`. We have both; render them
            // as the same `String` the JDK's `UnixFileKey.toString` produces so
            // two reads of the same file compare equal.
            "fileKey" => {
                if facts.dev == 0 && facts.ino == 0 {
                    Value::Object(None)
                } else {
                    let s = ctx
                        .create_string(&format!("(dev={:x},ino={})", facts.dev as u64, facts.ino));
                    Value::Object(Some(s))
                }
            }
            "mode" => box_int(ctx, facts.mode),
            "nlink" => box_int(ctx, facts.nlink),
            "uid" => box_int(ctx, facts.uid),
            "gid" => box_int(ctx, facts.gid),
            "dev" => box_long(ctx, facts.dev),
            "ino" => box_long(ctx, facts.ino),
            "rdev" => box_long(ctx, facts.rdev),
            "readonly" => box_boolean(ctx, facts.readonly),
            "hidden" => box_boolean(ctx, facts.hidden),
            // DOS-only flags with no Unix counterpart. `false` is what the JDK
            // reports for them on a non-DOS filesystem.
            "archive" | "system" => box_boolean(ctx, false),
            "permissions" => match posix_permission_set(ctx, facts.mode) {
                Some(set) => Value::Object(Some(set)),
                None => continue,
            },
            "owner" | "group" => {
                let Some((attrs, pin)) = owner_group.zip(attrs_pin) else {
                    continue;
                };
                let attrs = ctx.read_native_pin(pin, attrs);
                let (method, desc) = if name == "owner" {
                    ("owner", "()Ljava/nio/file/attribute/UserPrincipal;")
                } else {
                    ("group", "()Ljava/nio/file/attribute/GroupPrincipal;")
                };
                match ctx.invoke_virtual(attrs, method, desc, &[]) {
                    Ok(Some(v @ Value::Object(Some(_)))) => v,
                    // Not answerable on this platform/path. Omitting it from a
                    // `*` read matches what the JDK does for a view it cannot
                    // fully serve; an explicit request says so out loud.
                    _ if wildcard => continue,
                    _ => {
                        ctx.unpin_native_roots(map_pin);
                        return Err(RuntimeError::UnsupportedOperationException {
                            message: format!("'{view}:{name}' is not available for {path}"),
                        }
                        .into());
                    }
                }
            }
            _ => continue,
        };
        let key = ctx.read_native_pin(key_pin, key);
        let map = ctx.read_native_pin(map_pin, map);
        let put = ctx.invoke_virtual(
            map,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key)), value],
        );
        if let Err(e) = put {
            // Release the pin frame before unwinding: `?` here would leave the
            // map (and the attribute object) pinned for the rest of the VM's
            // life.
            ctx.unpin_native_roots(map_pin);
            return Err(e);
        }
    }

    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(map_pin);
    Ok(Some(Value::Object(Some(map))))
}

#[cfg(test)]
mod named_attribute_tests {
    use super::{
        attribute_names_for_view, attribute_write_kind, split_attribute_spec,
        supported_attribute_view_names, AttributeWrite,
    };

    /// `Files` treats a spec with no colon as the `basic` view. Getting this
    /// wrong turns `readAttributes(p, "size")` into a request for a view named
    /// `size`.
    #[test]
    fn a_spec_without_a_colon_is_the_basic_view() {
        assert_eq!(split_attribute_spec("size"), ("basic", "size"));
        assert_eq!(split_attribute_spec("*"), ("basic", "*"));
        assert_eq!(split_attribute_spec("unix:dev"), ("unix", "dev"));
        assert_eq!(split_attribute_spec("posix:*"), ("posix", "*"));
        // Only the FIRST colon splits; the JDK's own rule.
        assert_eq!(split_attribute_spec("unix:a:b"), ("unix", "a:b"));
    }

    /// Every view this VM advertises through `supportedFileAttributeViews()`
    /// must have a name table, or `readAttributes` answers
    /// `UnsupportedOperationException` for a view the same VM just claimed to
    /// support. `user` is the documented exception: it is the extended-attribute
    /// view, which has no fixed attribute names at all.
    #[test]
    fn every_advertised_view_except_user_has_a_name_table() {
        for view in supported_attribute_view_names() {
            if *view == "user" || *view == "acl" {
                continue;
            }
            assert!(
                attribute_names_for_view(view).is_some(),
                "view `{view}` is advertised by supportedFileAttributeViews() but \
                 readAttributes has no name table for it"
            );
        }
    }

    /// A wider view must be a superset of `basic`: the JDK's `PosixFileAttributes`
    /// and `UnixFileAttributes` extend `BasicFileAttributes`, so
    /// `readAttributes(p, "unix:*")` returning fewer keys than
    /// `readAttributes(p, "basic:*")` would be a silent regression for every
    /// caller that widened its view to get one extra field.
    #[test]
    fn posix_unix_and_dos_all_contain_the_basic_names() {
        let basic = attribute_names_for_view("basic").expect("basic");
        for view in ["posix", "unix", "dos"] {
            let names = attribute_names_for_view(view).expect(view);
            for b in basic {
                assert!(
                    names.contains(b),
                    "view `{view}` is missing basic attribute `{b}`"
                );
            }
        }
    }

    /// `setAttribute` and `readAttributes` must agree about what a view holds.
    /// Every name the reader answers gets a verdict from the writer — the
    /// default arm makes that trivially true, so what this really pins is WHICH
    /// verdict, i.e. that a name added to the reader's table is not silently
    /// classified writable (a write that lands somewhere unintended) or
    /// silently refused (a name `readAttributes` returns that `setAttribute`
    /// claims does not exist).
    #[test]
    fn the_writable_attributes_are_the_ones_the_jdk_lets_you_write() {
        for name in ["lastModifiedTime", "lastAccessTime", "creationTime"] {
            assert_eq!(attribute_write_kind(name), AttributeWrite::Time, "{name}");
        }
        for name in ["readonly", "hidden", "archive", "system"] {
            assert_eq!(attribute_write_kind(name), AttributeWrite::DosFlag, "{name}");
        }
        assert_eq!(
            attribute_write_kind("permissions"),
            AttributeWrite::Permissions
        );
        assert_eq!(attribute_write_kind("mode"), AttributeWrite::Mode);
        for name in ["owner", "group", "uid", "gid"] {
            assert_eq!(
                attribute_write_kind(name),
                AttributeWrite::Unimplemented,
                "{name}"
            );
        }
        // Read-only in the JDK: the basic attribute view's `setAttribute`
        // throws for every one of these.
        for name in [
            "size",
            "isRegularFile",
            "isDirectory",
            "isSymbolicLink",
            "isOther",
            "fileKey",
            "ino",
            "dev",
            "rdev",
            "nlink",
            "ctime",
        ] {
            assert_eq!(
                attribute_write_kind(name),
                AttributeWrite::NotWritable,
                "{name} must not be writable"
            );
        }

        // And every name any advertised view answers is covered by one of the
        // arms above — no name reaches the writer unclassified.
        for view in supported_attribute_view_names() {
            let Some(names) = attribute_names_for_view(view) else {
                continue;
            };
            for name in names {
                let kind = attribute_write_kind(name);
                assert!(
                    matches!(
                        kind,
                        AttributeWrite::Time
                            | AttributeWrite::DosFlag
                            | AttributeWrite::Permissions
                            | AttributeWrite::Mode
                            | AttributeWrite::Unimplemented
                            | AttributeWrite::NotWritable
                    ),
                    "`{view}:{name}` has no write verdict"
                );
            }
        }
    }

    /// The `unix` view is the one `sun.jvmstat.PlatformSupportImpl` reads during
    /// container detection, and that read is what made
    /// `com.sun.tools.attach.VirtualMachine.list()` throw `InternalError`.
    #[test]
    fn the_unix_view_carries_the_stat_fields_jvmstat_asks_for() {
        let unix = attribute_names_for_view("unix").expect("unix");
        for name in ["dev", "ino", "mode", "nlink", "uid", "gid", "rdev", "ctime"] {
            assert!(unix.contains(&name), "unix view is missing `{name}`");
        }
    }
}

fn box_long(ctx: &mut dyn NativeContext, v: i64) -> Value {
    crate::lang_class::box_value(ctx, Value::Long(v), "J")
}

fn box_int(ctx: &mut dyn NativeContext, v: i32) -> Value {
    crate::lang_class::box_value(ctx, Value::Int(v), "I")
}

fn box_boolean(ctx: &mut dyn NativeContext, v: bool) -> Value {
    crate::lang_class::box_value(ctx, Value::Int(i32::from(v)), "Z")
}

pub(crate) fn p59_files_read_attributes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Extract path from first argument (Path object, field 0 = string)
    let path_str = extract_path_string(ctx, args.first());

    // jar-FS path — attributes come from the archive listing, not the host
    // filesystem. The real-JDK `Files.walkFileTree` (FileTreeWalker) decides
    // dir-vs-file purely from this BFA; std::fs::metadata on the encoded
    // sentinel string ENOENTs, which made the walker treat every mounted-jar
    // root as a zero-length regular file (JUnit5 jar scanning found nothing).
    if let Some((jar, entry)) = jarfs_decode(&path_str) {
        let bfa = basic_file_attributes_alloc(ctx);
        let (is_dir, size) = match jarfs_classify(&jar, &entry) {
            JarFsKind::Dir => (1, 0i64),
            JarFsKind::File => (0, jarfs_entry_size(&jar, &entry).unwrap_or(0)),
            JarFsKind::Absent => {
                // Real readAttributes throws NoSuchFileException (an
                // IOException) for missing files; FileTreeWalker catches it
                // and reports visitFileFailed instead of walking garbage.
                return Err(RuntimeError::IOException {
                    message: format!("NoSuchFileException: {entry} in {jar}"),
                }
                .into());
            }
        };
        basic_file_attributes_store(ctx, bfa, is_dir != 0, size, 0, 0, 0, 0);
        return Ok(Some(Value::Object(Some(bfa))));
    }

    // jrt-FS (runtime image) path — attributes come from the jimage, not the
    // host filesystem. javac's platform-class indexing walks `/modules/...`.
    if let Some((java_home, entry)) = jrtfs_decode(&path_str) {
        let bfa = basic_file_attributes_alloc(ctx);
        let (is_dir, size) = match jrtfs_classify(&java_home, &entry) {
            JarFsKind::Dir => (1, 0i64),
            JarFsKind::File => (0, jrtfs_entry_size(&java_home, &entry).unwrap_or(0)),
            JarFsKind::Absent => {
                return Err(RuntimeError::IOException {
                    message: format!("NoSuchFileException: {entry} in jrt:/"),
                }
                .into());
            }
        };
        basic_file_attributes_store(ctx, bfa, is_dir != 0, size, 0, 0, 0, 0);
        return Ok(Some(Value::Object(Some(bfa))));
    }

    // NOFOLLOW_LINKS. `java.nio.file.LinkOption` is a single-constant enum, so
    // a non-empty `LinkOption[]` unambiguously means NOFOLLOW_LINKS — no
    // re-entrant `toString()` probe needed. This used to be an unimplemented
    // comment: every read followed links, so `isSymbolicLink()` could never be
    // true and a link's own size/timestamps were never visible.
    let nofollow = matches!(args.get(1), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
    let meta_result = if path_str.is_empty() {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "empty path",
        ))
    } else if nofollow {
        std::fs::symlink_metadata(&path_str)
    } else {
        std::fs::metadata(&path_str)
    };

    let bfa = basic_file_attributes_alloc(ctx);

    match meta_result {
        Ok(meta) => {
            // Creation time
            let creation_millis = meta.created().ok().map(system_time_to_millis).unwrap_or(0);
            // Last access time
            let access_millis = meta.accessed().ok().map(system_time_to_millis).unwrap_or(0);
            // Last modified time
            let mod_millis = meta.modified().ok().map(system_time_to_millis).unwrap_or(0);
            #[cfg(unix)]
            let perm_bits = {
                use std::os::unix::fs::PermissionsExt;
                (meta.permissions().mode() & 0o7777) as i32
            };
            #[cfg(not(unix))]
            let perm_bits = 0i32;
            basic_file_attributes_store(
                ctx,
                bfa,
                meta.is_dir(),
                meta.len() as i64,
                creation_millis,
                access_millis,
                mod_millis,
                perm_bits,
            );
            // Record link-ness so `isSymbolicLink()` can stop answering a
            // hardcoded `false`. Only reachable under NOFOLLOW_LINKS, since a
            // following read resolves the target (and the real JDK likewise
            // reports `isSymbolicLink() == false` there).
            if meta.file_type().is_symlink() {
                if basic_file_attributes_is_synthetic(ctx, bfa) {
                    // Synthetic stub has no named fields — patch the slot.
                    let cur = basic_file_attributes_syn_mode(ctx, bfa);
                    ctx.set_field(
                        bfa,
                        BFA_SYN_SLOT_MODE,
                        Value::Int((cur & !UNIX_S_IFMT) | UNIX_S_IFLNK),
                    );
                } else if basic_file_attributes_is_windows(ctx, bfa) {
                    let cur = match ctx.get_field_by_name(bfa, "fileAttrs") {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    ctx.set_field_by_name(
                        bfa,
                        "fileAttrs",
                        Value::Int(cur | WIN_ATTR_REPARSE_POINT),
                    );
                } else {
                    let cur = match ctx.get_field_by_name(bfa, "st_mode") {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    ctx.set_field_by_name(
                        bfa,
                        "st_mode",
                        Value::Int((cur & !UNIX_S_IFMT) | UNIX_S_IFLNK),
                    );
                }
            }
            // `UnixFileAttributes.owner()`/`group()` (real JDK bytecode) read
            // these two fields and hand them to `UnixUserPrincipals.fromUid/
            // fromGid`. They were never populated, so every file reported
            // uid/gid 0 ("root") — wrong for any ownership check, and the
            // reason Spring Boot's `ApplicationTemp` ownership assertion had
            // nothing sensible to compare against. Only meaningful on Unix;
            // the Windows attribute object has no such fields (the setters
            // are by-name and are silently dropped when absent).
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ctx.set_field_by_name(bfa, "st_uid", Value::Int(meta.uid() as i32));
                ctx.set_field_by_name(bfa, "st_gid", Value::Int(meta.gid() as i32));
                // `st_dev`/`st_ino` ARE the file's identity — both our
                // `fileKey()` native and the real JDK's own
                // `UnixFileAttributes.fileKey()` build a key out of them.
                // Never populated before, which is why `fileKey()` had nothing
                // to report and returned null for every file.
                ctx.set_field_by_name(bfa, "st_dev", Value::Long(meta.dev() as i64));
                ctx.set_field_by_name(bfa, "st_ino", Value::Long(meta.ino() as i64));
            }
            // Windows equivalent of the `st_dev`/`st_ino` identity above: the
            // `(volume serial, file index)` triple `WindowsFileAttributes`
            // carries. Unlike the Unix fields this needs a second handle open,
            // so restrict it to directories and links — `FileTreeWalker`'s
            // loop check (the reason `fileKey()` has to be real) only ever asks
            // about those, and a tree walk over a large source tree should not
            // pay an extra CreateFile per ordinary file.
            #[cfg(windows)]
            {
                if meta.is_dir() || meta.file_type().is_symlink() {
                    if let Some((volume, high, low)) = win_file_identity(&path_str) {
                        ctx.set_field_by_name(bfa, "volSerialNumber", Value::Int(volume as i32));
                        ctx.set_field_by_name(bfa, "fileIndexHigh", Value::Int(high as i32));
                        ctx.set_field_by_name(bfa, "fileIndexLow", Value::Int(low as i32));
                    }
                }
            }
        }
        // NIO contract: `Files.readAttributes` must raise `IOException`
        // (`NoSuchFileException` when the path is missing) — silently
        // answering with a fake all-zero `BasicFileAttributes` masked every
        // failure, including a missing file. Real bytecode
        // `Files.getLastModifiedTime`/`size`/`isDirectory` etc. are thin
        // wrappers over `readAttributes`, so this silent success also broke
        // `Files.getLastModifiedTime` never throwing for a missing file —
        // `FileSystemResource.lastModified()` catches `NoSuchFileException`
        // and translates it to `FileNotFoundException`
        // (ResourceTests#resourceCreateRelativeUnknown).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(p57_no_such_file(ctx, &path_str));
        }
        Err(e) => return Err(p57_io_error(&e)),
    }

    Ok(Some(Value::Object(Some(bfa))))
}

// =============================================================================
// java.nio.file — Files.exists/isDirectory, Path expansion
// =============================================================================

pub(crate) fn register_p61_files_path(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let files = "java/nio/file/Files";
    r.register(
        files,
        "exists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| Ok(Some(Value::Int(i32::from(p57_files_exists_impl(ctx, args))))),
    );
    r.register(
        files,
        "notExists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| Ok(Some(Value::Int(i32::from(!p57_files_exists_impl(ctx, args))))),
    );
    r.register(
        files,
        "isDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| Ok(Some(Value::Int(i32::from(p57_files_is_directory_impl(ctx, args))))),
    );
    r.register(
        files,
        "isRegularFile",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            Ok(Some(Value::Int(i32::from(p57_files_is_regular_file_impl(
                ctx, args,
            )))))
        },
    );
    r.register(files, "size", "(Ljava/nio/file/Path;)J", p59_files_size);
    r.register(
        files,
        "isReadable",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            if let Some(Value::Object(Some(path_ref))) = args.first() {
                let path_str = match ctx.get_field(*path_ref, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => return Ok(Some(Value::Int(0))),
                };
                let readable = match vfs_classify(&path_str) {
                    Some(kind) => !matches!(kind, JarFsKind::Absent),
                    None => std::path::Path::new(&path_str).exists(),
                };
                Ok(Some(Value::Int(if readable { 1 } else { 0 })))
            } else {
                Ok(Some(Value::Int(0)))
            }
        },
    );
    r.register(
        files,
        "isWritable",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            if let Some(Value::Object(Some(path_ref))) = args.first() {
                let path_str = match ctx.get_field(*path_ref, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => return Ok(Some(Value::Int(0))),
                };
                let writable = std::fs::metadata(&path_str)
                    .map(|m| !m.permissions().readonly())
                    .unwrap_or(false);
                Ok(Some(Value::Int(if writable { 1 } else { 0 })))
            } else {
                Ok(Some(Value::Int(0)))
            }
        },
    );

    // --- Path expansion ---
    //
    // NO `java/nio/file/Path` REGISTRATIONS BELONG HERE. This function used to
    // re-register six of them — `getFileName`, `getParent`, `toAbsolutePath`,
    // `resolve(String)`, `resolve(Path)`, `getNameCount` — all of which
    // `register_phase57_nio_file` already owns. `NativeMethodRegistry::register`
    // overwrites the slot in place, so the LAST registration wins, and phase 61
    // runs after phase 57 (`register_all_natives`): every one of these silently
    // took over from the phase-57 implementation wherever that path is used
    // (synthetic-JDK mode; the shipping real-JDK VM calls
    // `register_phase57_nio_file` directly from `vm_init.rs` and never gets
    // here, which is the only reason this went unnoticed).
    //
    // They were not equivalent. The `resolve` pair joined with
    // `std::path::Path::join` and wrote the result straight into the object
    // instead of going through `p57_alloc_path`, so they skipped the
    // normalize-at-construction step that `sun.nio.fs.UnixPath`/`WindowsPath`
    // perform — which is exactly the trailing-separator defect
    // `fixed-suite-bugs/springboot/resourcestests-trailing-slash-path-normalization-FIXED-20260804.md`
    // was filed for, re-introduced one phase later. `getFileName` used
    // `jarfs_decode` (missing jrt) and dropped the root/null contract;
    // `getNameCount` and `getParent` had already been hand-synced to their
    // phase-57 twins by earlier sessions, which is a standing invitation to
    // drift.
    //
    // Deleting them is strictly better than keeping them identical: there is
    // one implementation, and phase order stops being load-bearing. See
    // `p61_registers_no_duplicate_path_natives` in `registry_contracts.rs`,
    // which fails if any of them come back.
    r.set_category(__prev_cat);
}

// URI/URL already fully implemented in earlier phases — no additions needed
pub(crate) fn register_p61_net(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // NetworkInterface = 5-field synthetic (name=0, displayName=1, addresses=2, index=3, flags=4)
    // flags: bit 0 = up, bit 1 = loopback, bit 2 = supportsMulticast
    let ni = "java/net/NetworkInterface";

    // NOTE: `getNetworkInterfaces` is intentionally NOT registered as a
    // synthetic native in real-JDK mode. See the long comment in
    // `register_re8_network_interface` (net_phase_e.rs) for details. The
    // real JDK Java method calls native `getAll()` (returns empty) which
    // produces a SocketException that callers handle. Returning synthetic
    // NetworkInterface objects here breaks downstream `getInetAddresses()`
    // because the real JDK reads field slots we don't populate.

    r.register(
        ni,
        "getByName",
        "(Ljava/lang/String;)Ljava/net/NetworkInterface;",
        |ctx, args| {
            let name_ref = obj_arg(args, 1)?;
            let name = ctx.read_string(name_ref).unwrap_or_default();
            let interfaces = p61_build_network_interfaces(ctx);
            for iface in &interfaces {
                if let Value::Object(Some(s)) = ctx.get_field(*iface, 0) {
                    if ctx.read_string(s).as_deref() == Some(&name) {
                        return Ok(Some(Value::Object(Some(*iface))));
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

    r.register(
        ni,
        "getByInetAddress",
        "(Ljava/net/InetAddress;)Ljava/net/NetworkInterface;",
        |ctx, args| {
            let addr_ref = obj_arg(args, 1)?;
            let addr_str = match ctx.get_field(addr_ref, 1) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let interfaces = p61_build_network_interfaces(ctx);
            for iface in &interfaces {
                if let Value::Object(Some(addrs)) = ctx.get_field(*iface, 2) {
                    let len = ctx.array_length(addrs);
                    for i in 0..len {
                        if let Value::Object(Some(ia)) = ctx.get_array_element(addrs, i) {
                            if let Value::Object(Some(s)) = ctx.get_field(ia, 1) {
                                if ctx.read_string(s).as_deref() == Some(&addr_str) {
                                    return Ok(Some(Value::Object(Some(*iface))));
                                }
                            }
                        }
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

    r.register(ni, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(ni, "getDisplayName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        ni,
        "getInetAddresses",
        "()Ljava/util/Enumeration;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addrs = ctx.get_field(this, 2);
            // Concrete `Enumeration$Impl`, not the bare `Enumeration` interface.
            let enum_obj = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
            match addrs {
                Value::Object(Some(a)) => {
                    ctx.set_field(enum_obj, 0, Value::Object(Some(a)));
                    ctx.set_field(enum_obj, 1, Value::Int(0));
                }
                _ => {
                    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                    ctx.set_field(enum_obj, 0, Value::Object(Some(empty)));
                    ctx.set_field(enum_obj, 1, Value::Int(0));
                }
            }
            Ok(Some(Value::Object(Some(enum_obj))))
        },
    );
    r.register(ni, "getIndex", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(ni, "isUp", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flags = ctx.get_field(this, 4).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 1 != 0 { 1 } else { 0 })))
    });
    r.register(ni, "isLoopback", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flags = ctx.get_field(this, 4).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 2 != 0 { 1 } else { 0 })))
    });
    r.register(ni, "supportsMulticast", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flags = ctx.get_field(this, 4).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 4 != 0 { 1 } else { 0 })))
    });
    // Was a hardcoded `false` ("IFF_POINTOPOINT has no portable query"). Linux
    // publishes the interface's raw IFF_* word as a plain file, the same
    // bitmask `SIOCGIFFLAGS` returns and the same one the JDK's
    // `NetworkInterface.isP2P()` tests — read it, exactly as the `getMTU`
    // registration below reads `/sys/class/net/<name>/mtu`. Falls back to
    // `false` where `/sys` is unavailable (Windows/macOS) or the synthesized
    // name has no real interface behind it, which is the same answer as
    // before but now only when we genuinely cannot tell.
    r.register(ni, "isPointToPoint", "()Z", |ctx, args| {
        const IFF_POINTOPOINT: u32 = 0x10;
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if !name.is_empty() && !name.contains('/') && !name.contains('\\') {
            if let Ok(text) = std::fs::read_to_string(format!("/sys/class/net/{name}/flags")) {
                let raw = text.trim();
                let digits = raw.strip_prefix("0x").unwrap_or(raw);
                if let Ok(flags) = u32::from_str_radix(digits, 16) {
                    return Ok(Some(Value::Int(i32::from(flags & IFF_POINTOPOINT != 0))));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    });
    // Was a hardcoded `false`. The JDK defines a "virtual" interface as a
    // sub-interface, which is exactly the `eth0:1` naming convention — read it
    // off the name (slot 0) rather than asserting none exist.
    r.register(ni, "isVirtual", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(Some(Value::Int(i32::from(name.contains(':')))))
    });
    // STUB-REMOVAL (wave 3). This used to hand back a FABRICATED all-zero MAC
    // (00:00:00:00:00:00) because "real MAC requires platform APIs". That is
    // strictly worse than admitting we don't know: 00:00:00:00:00:00 is a
    // syntactically valid address, so callers that derive a node ID from it
    // (UUID v1 generators, cluster member identity, licence fingerprints) get
    // a plausible-looking constant instead of a signal to fall back — and
    // every host in a cluster gets the SAME one.
    //
    // `null` is the spec'd answer: getHardwareAddress returns null when the
    // address does not exist or is not accessible. It is also what HotSpot 25
    // returns for the loopback interface, verified on Windows, and loopback is
    // the interface `p61_build_network_interfaces` always synthesizes.
    //
    // WAVE-4: an unconditional `null` is still an answer we do not have to
    // guess at on Linux, which publishes the real MAC as a plain file — the
    // same source `getMTU`/`isPointToPoint` read. Return the real address when
    // `/sys` has one, and keep `null` for the two cases the spec names: no
    // address (loopback publishes the all-zero one, which is exactly "does not
    // exist" and is what HotSpot reports as null) and not accessible.
    r.register(ni, "getHardwareAddress", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Ok(Some(Value::Object(None)));
        }
        let text = match std::fs::read_to_string(format!("/sys/class/net/{name}/address")) {
            Ok(t) => t,
            Err(_) => return Ok(Some(Value::Object(None))),
        };
        let mut bytes: Vec<u8> = Vec::new();
        for octet in text.trim().split(':') {
            match u8::from_str_radix(octet, 16) {
                Ok(b) => bytes.push(b),
                Err(_) => return Ok(Some(Value::Object(None))),
            }
        }
        // All-zero (loopback) means "no hardware address" -> null, per spec.
        if bytes.is_empty() || bytes.iter().all(|&b| b == 0) {
            return Ok(Some(Value::Object(None)));
        }
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
    // Was a flat 1500 for every interface — wrong for the loopback interface on
    // every platform (Linux `lo` is 65536), which is the one interface
    // `p61_build_network_interfaces` ALWAYS synthesizes. Linux publishes the
    // real per-interface MTU as a plain file, the same number `SIOCGIFMTU`
    // returns, so read it; elsewhere fall back to the loopback/Ethernet
    // defaults rather than one constant for both.
    r.register(ni, "getMTU", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if !name.is_empty() && !name.contains('/') && !name.contains('\\') {
            if let Ok(text) = std::fs::read_to_string(format!("/sys/class/net/{name}/mtu")) {
                if let Ok(mtu) = text.trim().parse::<i32>() {
                    return Ok(Some(Value::Int(mtu)));
                }
            }
        }
        // Fallback when /sys is unavailable. The loopback MTU is PLATFORM
        // SPECIFIC, not a universal 65536: Linux `lo` is 65536, but Windows
        // loopback reports 1500 and macOS lo0 reports 16384. Measured against
        // HotSpot 25 on Windows, which answers 1500 — so a flat 65536 here
        // would diverge from the reference JVM on that host, which the
        // previous flat 1500 happened to match.
        let loopback = ctx.get_field(this, 4).as_int().unwrap_or(0) & 2 != 0;
        let mtu = if !loopback {
            1500
        } else if cfg!(target_os = "linux") {
            65536
        } else if cfg!(target_os = "macos") {
            16384
        } else {
            1500
        };
        Ok(Some(Value::Int(mtu)))
    });
    r.register(
        ni,
        "getSubInterfaces",
        "()Ljava/util/Enumeration;",
        |ctx, _args| {
            // Concrete `Enumeration$Impl`, not the bare `Enumeration` interface.
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let enum_obj = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
            Ok(Some(Value::Object(Some(enum_obj))))
        },
    );
    // Was an unconditional `null`, justified by "getSubInterfaces() never
    // yields any". That reasons about the wrong direction: `getParent()` is
    // asked of the SUB-interface, and this VM already recognises one — the
    // `isVirtual()` registration above defines a sub-interface exactly as the
    // JDK does, by the `eth0:1` naming convention. So a receiver named
    // `eth0:1` does have a parent (`eth0`), and answering null for it
    // contradicted `isVirtual()`. Resolve it the same way `getByName` does.
    r.register(
        ni,
        "getParent",
        "()Ljava/net/NetworkInterface;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // Not a sub-interface -> no parent (the documented JDK answer).
            let parent_name = match name.split_once(':') {
                Some((base, _)) if !base.is_empty() => base.to_string(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let interfaces = p61_build_network_interfaces(ctx);
            for iface in &interfaces {
                if let Value::Object(Some(s)) = ctx.get_field(*iface, 0) {
                    if ctx.read_string(s).as_deref() == Some(&parent_name) {
                        return Ok(Some(Value::Object(Some(*iface))));
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(ni, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let n1 = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let n2 = match ctx.get_field(other, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(Some(Value::Int(if n1 == n2 { 1 } else { 0 })))
    });
    r.register(ni, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let mut hash: i32 = 0;
        for b in name.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(b as i32);
        }
        Ok(Some(Value::Int(hash)))
    });
    r.register(ni, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let s = ctx.create_string(&format!("name:{}", name));
        Ok(Some(Value::Object(Some(s))))
    });
    r.set_category(__prev_cat);
}

/// Build the list of NetworkInterface synthetic objects for this host.
/// Returns loopback + primary interface (resolved via hostname DNS).
pub(crate) fn p61_build_network_interfaces(ctx: &mut dyn NativeContext) -> Vec<ObjectRef> {
    let mut result = Vec::new();

    // 1. Loopback interface
    let lo = alloc_concurrent_synthetic(ctx, "java/net/NetworkInterface", 5);
    let lo_name = ctx.create_string("lo");
    let lo_display = ctx.create_string("Loopback Interface");
    ctx.set_field(lo, 0, Value::Object(Some(lo_name)));
    ctx.set_field(lo, 1, Value::Object(Some(lo_display)));
    // InetAddress for 127.0.0.1 — `alloc_inet_address_external` records
    // host/IP in the `net_phase_e` side table AND populates a real-JDK
    // `InetAddress$InetAddressHolder` (instance slot 0 is the typed
    // `holder` reference field, never a bare String). This is the object
    // Hazelcast's `DefaultAddressPicker` enumerates via
    // `NetworkInterface.getInetAddresses()` then calls `.getHostName()` on.
    let lo_addr = crate::net_phase_e::alloc_inet_address_external(ctx, "localhost", "127.0.0.1");
    let lo_addrs = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(lo_addrs, 0, Value::Object(Some(lo_addr)));
    ctx.set_field(lo, 2, Value::Object(Some(lo_addrs)));
    ctx.set_field(lo, 3, Value::Int(1)); // index
    ctx.set_field(lo, 4, Value::Int(0b111)); // up + loopback + multicast
    result.push(lo);

    // 2. Primary interface — resolve hostname to get primary IP. Use the
    //    portable hostname/IP helpers so this code path agrees with what
    //    `InetAddress.getLocalHost` returns and works identically on Linux,
    //    macOS, and Windows. (NEW-2 / C7)
    let hostname = crate::resolve_real_hostname();
    let primary_ip = crate::resolve_primary_ipv4(&hostname);

    if primary_ip != "127.0.0.1" {
        let eth = alloc_concurrent_synthetic(ctx, "java/net/NetworkInterface", 5);
        let eth_name = ctx.create_string("eth0");
        let eth_display = ctx.create_string("Primary Network Interface");
        ctx.set_field(eth, 0, Value::Object(Some(eth_name)));
        ctx.set_field(eth, 1, Value::Object(Some(eth_display)));
        let eth_addr = crate::net_phase_e::alloc_inet_address_external(ctx, &hostname, &primary_ip);
        let eth_addrs = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(eth_addrs, 0, Value::Object(Some(eth_addr)));
        ctx.set_field(eth, 2, Value::Object(Some(eth_addrs)));
        ctx.set_field(eth, 3, Value::Int(2)); // index
        ctx.set_field(eth, 4, Value::Int(0b101)); // up + multicast (not loopback)
        result.push(eth);
    }

    result
}

// =============================================================================
// java.nio.file.FileVisitor and SimpleFileVisitor stubs
// =============================================================================

pub(crate) fn register_p66_file_visitor(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sfv = "java/nio/file/SimpleFileVisitor";
    // SimpleFileVisitor has default methods that return CONTINUE
    r.register(sfv, "preVisitDirectory", "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;", |ctx, _args| {
        let result = p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0)?;
        Ok(result)
    });
    r.register(sfv, "visitFile", "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;", |ctx, _args| {
        let result = p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0)?;
        Ok(result)
    });
    r.register(
        sfv,
        "visitFileFailed",
        "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
        |ctx, _args| {
            let result = p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0)?;
            Ok(result)
        },
    );
    r.register(
        sfv,
        "postVisitDirectory",
        "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
        |ctx, _args| {
            let result = p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0)?;
            Ok(result)
        },
    );

    // FileVisitResult enum
    let fvr = "java/nio/file/FileVisitResult";
    r.register(
        fvr,
        "values",
        "()[Ljava/nio/file/FileVisitResult;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
            let names = ["CONTINUE", "TERMINATE", "SKIP_SUBTREE", "SKIP_SIBLINGS"];
            for (i, name) in names.iter().enumerate() {
                if let Ok(Some(e)) =
                    p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", name, i as i32)
                {
                    ctx.set_array_element(arr, i, e);
                }
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        fvr,
        "valueOf",
        "(Ljava/lang/String;)Ljava/nio/file/FileVisitResult;",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => "CONTINUE".to_string(),
            };
            let ordinal = match name.as_str() {
                "CONTINUE" => 0,
                "TERMINATE" => 1,
                "SKIP_SUBTREE" => 2,
                "SKIP_SIBLINGS" => 3,
                _ => 0,
            };
            p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", &name, ordinal)
        },
    );

    // Files.walkFileTree — real directory walking with visitor callbacks
    r.register(
        "java/nio/file/Files",
        "walkFileTree",
        "(Ljava/nio/file/Path;Ljava/nio/file/FileVisitor;)Ljava/nio/file/Path;",
        // The 2-arg overload is specified as "does not follow symbolic links".
        |ctx, args| p98_walk_file_tree(ctx, args, usize::MAX, false),
    );
    r.register(
        "java/nio/file/Files",
        "walkFileTree",
        "(Ljava/nio/file/Path;Ljava/util/Set;ILjava/nio/file/FileVisitor;)Ljava/nio/file/Path;",
        |ctx, args| {
            // Options first: the `Set<FileVisitOption>` probe calls `isEmpty()`
            // bytecode, which can allocate, and both the root path and the
            // visitor copied out of `args` would be stale locals after it.
            let follow = p57_visit_options_follow_links(ctx, args.get(1));
            let path_obj = args.first().copied().unwrap_or(Value::Object(None));
            let requested_depth = args.get(2).and_then(Value::as_int).unwrap_or(i32::MAX);
            let max_depth = if requested_depth == i32::MAX {
                usize::MAX
            } else {
                requested_depth.max(0) as usize
            };
            let visitor = args.get(3).copied().unwrap_or(Value::Object(None));
            p98_walk_file_tree(ctx, &[path_obj, visitor], max_depth, follow)
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) fn p98_walk_file_tree(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    max_depth: usize,
    follow_links: bool,
) -> MethodCallResult {
    let path_val = args.first().copied().unwrap_or(Value::Object(None));
    let visitor = if let Some(Value::Object(Some(v))) = args.get(1) {
        *v
    } else {
        return Ok(Some(path_val));
    };
    let path_obj = if let Value::Object(Some(p)) = path_val {
        p
    } else {
        return Ok(Some(path_val));
    };
    let root_str = match ctx.get_field(path_obj, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return Ok(Some(path_val)),
    };
    let visitor_class_id = ctx.class_id_of_object(visitor);
    let visitor_class_name = ctx.class_name_of_id(visitor_class_id);
    let skip_file_callbacks = visitor_class_name
        .as_deref()
        .is_some_and(|name| name == "com/sun/tools/javac/file/JavacFileManager$ArchiveContainer$1");
    if crate::nbflags().dbg_visitfile {
        eprintln!(
            "[p98-walkfiletree] visitor_class_id={:?} visitor_class_name={:?} skip_file_callbacks={}",
            visitor_class_id, visitor_class_name, skip_file_callbacks
        );
    }

    // GC-safety: the walk below drives a re-entrant, potentially deep and
    // long-running sequence of Java callbacks (preVisitDirectory/visitFile/
    // postVisitDirectory) for every directory and file under `root`. Any of
    // those calls can allocate and trigger GC (directly, or transitively --
    // e.g. `PathFileObject.forJarPath` inside a compiler's own visitor).
    // `visitor` is a single Rust-local `ObjectRef` that stays alive across
    // the ENTIRE recursive walk; under a moving collector a mid-walk
    // relocation leaves a bare local like this stale, and `invoke_virtual`'s
    // own `load_and_forward` cannot repair it once the old slot has been
    // reused for an unrelated (often array) allocation -- silently
    // redirecting dispatch to `java.lang.Object` and raising a spurious
    // `NoSuchMethodError` on the visitor's real method. Confirmed live: a
    // very large in-memory javac classpath walk (`BeanRegistrationsAot-
    // ContributionTests`, `JavacFileManager$ArchiveContainer.list`'s own
    // `SimpleFileVisitor`) reproduced exactly this signature --
    // `NoSuchMethodError: java/lang/Object.visitFile(...)`.
    //
    // Pin `visitor` and the root `path_obj` (used at both ends of the walk)
    // for the whole traversal via `p98_pin`/`p98_read_pin` (see their doc
    // comment below) and unpin the complete batch -- every pin taken
    // anywhere during the walk, since `visitor_pin` is the first one pushed
    // -- once it returns.
    let visitor_pin = p98_pin(ctx, visitor);
    let path_pin = p98_pin(ctx, path_obj);
    // JDK 25's ArchiveContainer indexer only records package directories.
    // Serve that exact, already-indexed jar walk without an interpreter
    // callback for every directory; all other visitors use the generic path.
    if skip_file_callbacks
        && max_depth == usize::MAX
        && p98_index_javac_archive_packages(ctx, &root_str, visitor_pin, path_pin)?
    {
        ctx.unpin_native_roots(visitor_pin.0);
        return Ok(Some(path_val));
    }
    // A root that is not a directory (a plain file, or — without FOLLOW_LINKS —
    // a symbolic link, even one pointing at a directory) gets a single
    // `visitFile` callback, not the preVisitDirectory/postVisitDirectory pair.
    // Treating a symlink root as a directory is what made
    // `FileSystemUtils.deleteRecursively(symlink)` recurse into the link's
    // TARGET and then fail to unlink the link itself (`ApplicationTempTests`,
    // whose leftover symlink then poisoned every later test in the class).
    let result = if vfs_or_host_is_walkable_dir(&root_str, follow_links) {
        p98_walk_dir(
            ctx,
            &root_str,
            visitor_pin,
            path_pin,
            skip_file_callbacks,
            max_depth,
            follow_links,
        )
        .map(|_| ())
    } else {
        p98_visit_single_file(ctx, &root_str, visitor_pin, path_pin, skip_file_callbacks)
    };
    ctx.unpin_native_roots(visitor_pin.0);
    result?;
    Ok(Some(path_val))
}

/// `visitFile` for a walk root that is not a directory. `Files.walkFileTree`
/// accepts any path; the JDK reports a non-directory root through exactly one
/// `visitFile` callback and never touches the directory callbacks.
fn p98_visit_single_file(
    ctx: &mut dyn NativeContext,
    path: &str,
    visitor_pin: P98Pin,
    path_pin: P98Pin,
    skip_file_callbacks: bool,
) -> Result<(), MethodCallFailed> {
    if skip_file_callbacks {
        return Ok(());
    }
    // A symlink's own size, not its target's — the walk did not follow it.
    let size = std::fs::symlink_metadata(path)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    let fa = p98_alloc_basic_file_attributes(ctx, false, size);
    let visitor_now = p98_read_pin(ctx, visitor_pin);
    let path_now = p98_read_pin(ctx, path_pin);
    p98_invoke_file_visitor(
        ctx,
        visitor_now,
        "visitFile",
        "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
        "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
        path_now,
        Value::Object(Some(fa)),
    )?;
    Ok(())
}

pub(crate) fn p98_is_missing_visitor_method(
    err: &MethodCallFailed,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Linkage(LinkageError::NoSuchMethodError {
            method_name: missing_name,
            method_descriptor,
            ..
        })) if missing_name == method_name && method_descriptor == descriptor
    )
}

pub(crate) fn p98_invoke_file_visitor(
    ctx: &mut dyn NativeContext,
    visitor: ObjectRef,
    method_name: &str,
    concrete_descriptor: &str,
    erased_descriptor: &str,
    path_obj: ObjectRef,
    second_arg: Value,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let args = [Value::Object(Some(path_obj)), second_arg];
    let result = ctx.invoke_virtual(visitor, method_name, erased_descriptor, &args);
    let value = match result {
        Ok(value) => value,
        Err(err) if p98_is_missing_visitor_method(&err, method_name, erased_descriptor) => {
            ctx.invoke_virtual(visitor, method_name, concrete_descriptor, &args)?
        }
        Err(err) => return Err(err),
    };
    Ok(match value {
        Some(Value::Object(Some(result))) => Some(result),
        _ => None,
    })
}

/// A native-pinned GC root plus its original (possibly later stale) value,
/// kept as the fallback `read_native_pin` returns for `NativeContext`
/// implementations that don't support pinning (e.g. test mocks -- see
/// [`NativeContext::pin_native_root`]'s doc comment). Create one with
/// `p98_pin` right after allocating/receiving the object and resolve the
/// current, GC-forwarded reference with `p98_read_pin` immediately before
/// each re-entrant use -- never hold the raw `ObjectRef` itself across a
/// call that can allocate.
pub(crate) type P98Pin = (usize, ObjectRef);

pub(crate) fn p98_pin(ctx: &mut dyn NativeContext, obj: ObjectRef) -> P98Pin {
    (ctx.pin_native_root(obj), obj)
}

pub(crate) fn p98_read_pin(ctx: &dyn NativeContext, pin: P98Pin) -> ObjectRef {
    ctx.read_native_pin(pin.0, pin.1)
}

/// Build a concrete platform `BasicFileAttributes` implementation for a
/// `Files.walkFileTree` visitor callback. The named-field bridge makes the
/// representation independent from the platform class's physical field order.
pub(crate) fn p98_alloc_basic_file_attributes(
    ctx: &mut dyn NativeContext,
    is_dir: bool,
    size: i64,
) -> ObjectRef {
    let attrs = basic_file_attributes_alloc(ctx);
    basic_file_attributes_store(ctx, attrs, is_dir, size, 0, 0, 0, 0);
    attrs
}

/// Fast-path the only callback implemented by javac's archive indexer.
///
/// JDK 25's `JavacFileManager$ArchiveContainer$1` captures an
/// `ArchiveContainer` and its root path, then adds every directory with a
/// Java-identifier basename to `ArchiveContainer.packages`. The archive
/// filesystem already has the complete direct-child tree in [`JarFsIndex`],
/// so reproducing that narrow operation avoids a full interpreter round-trip
/// for every directory of every classpath jar. Return `false` for any shape
/// we do not recognize so generic visitor dispatch remains the fallback.
pub(crate) fn p98_index_javac_archive_packages(
    ctx: &mut dyn NativeContext,
    root: &str,
    visitor_pin: P98Pin,
    root_path_pin: P98Pin,
) -> Result<bool, MethodCallFailed> {
    let Some((jar, root_entry)) = jarfs_decode(root) else {
        return Ok(false);
    };
    let Some(index) = jar_index(&jar) else {
        return Ok(false);
    };

    // Keep Unicode package names on the generic path. The direct predicate
    // covers the common ASCII archive layout; falling back rather than
    // approximating Java's Unicode identifier rules preserves correctness for
    // unusual jars.
    if index.children.keys().any(|directory| !directory.is_ascii()) {
        return Ok(false);
    }

    let visitor = p98_read_pin(ctx, visitor_pin);
    let container = match ctx.get_field(visitor, 1) {
        Value::Object(Some(container))
            if ctx
                .class_name_of_id(ctx.class_id_of_object(container))
                .as_deref()
                == Some("com/sun/tools/javac/file/JavacFileManager$ArchiveContainer") =>
        {
            container
        }
        _ => return Ok(false),
    };
    let packages = match ctx.get_field(container, 2) {
        Value::Object(Some(packages)) => packages,
        _ => return Ok(false),
    };
    let packages_pin = p98_pin(ctx, packages);
    let packages_base = packages_pin.0;

    // `(archive entry, path relative to the walk root)`. The visitor adds the
    // root itself under RelativeDirectory("") and prunes a non-identifier
    // directory (for example META-INF) together with its subtree.
    let root_entry = root_entry.trim_start_matches('/').trim_end_matches('/');
    let mut pending = vec![(root_entry.to_string(), String::new(), true)];
    while let Some((entry, relative, is_root)) = pending.pop() {
        if !is_root
            && !entry
                .rsplit('/')
                .next()
                .is_some_and(p98_is_ascii_java_identifier)
        {
            continue;
        }

        let path_pin = if is_root {
            root_path_pin
        } else {
            let encoded = jarfs_encode(&jar, &entry);
            let path = alloc_concurrent_synthetic(ctx, "java/nio/file/Path", 2);
            let path_pin = p98_pin(ctx, path);
            let path_string = ctx.create_string(&encoded);
            let path_now = p98_read_pin(ctx, path_pin);
            ctx.set_field(path_now, 0, Value::Object(Some(path_string)));
            path_pin
        };
        let relative_string = ctx.create_string(&relative);
        let relative_string_pin = p98_pin(ctx, relative_string);
        let relative_directory = match ctx.new_object_initialized(
            "com/sun/tools/javac/file/RelativePath$RelativeDirectory",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(p98_read_pin(ctx, relative_string_pin)))],
        )? {
            Some(Value::Object(Some(directory))) => directory,
            _ => {
                ctx.unpin_native_roots(packages_base);
                return Ok(false);
            }
        };
        let directory_pin = p98_pin(ctx, relative_directory);
        let packages_now = p98_read_pin(ctx, packages_pin);
        let directory_now = p98_read_pin(ctx, directory_pin);
        let path_now = p98_read_pin(ctx, path_pin);
        ctx.invoke_virtual(
            packages_now,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(directory_now)),
                Value::Object(Some(path_now)),
            ],
        )?;

        if let Some(children) = index.children.get(&entry) {
            for (child, is_dir) in children.iter().rev() {
                if *is_dir {
                    let leaf = child.rsplit('/').next().unwrap_or_default();
                    let child_relative = if relative.is_empty() {
                        leaf.to_string()
                    } else {
                        format!("{relative}/{leaf}")
                    };
                    pending.push((child.clone(), child_relative, false));
                }
            }
        }
    }
    ctx.unpin_native_roots(packages_base);
    Ok(true)
}

pub(crate) fn p98_is_ascii_java_identifier(component: &str) -> bool {
    let mut chars = component.bytes();
    let Some(first) = chars.next() else {
        return false;
    };
    matches!(first, b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'$')
        && chars.all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$'))
}

pub(crate) fn p98_walk_dir(
    ctx: &mut dyn NativeContext,
    dir: &str,
    visitor_pin: P98Pin,
    dir_path_pin: P98Pin,
    skip_file_callbacks: bool,
    remaining_depth: usize,
    follow_links: bool,
) -> Result<bool, MethodCallFailed> {
    let attrs = p98_alloc_basic_file_attributes(ctx, true, 0);
    // preVisitDirectory
    let visitor_now = p98_read_pin(ctx, visitor_pin);
    let dir_path_now = p98_read_pin(ctx, dir_path_pin);
    let pre = p98_invoke_file_visitor(
        ctx,
        visitor_now,
        "preVisitDirectory",
        "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
        "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
        dir_path_now,
        Value::Object(Some(attrs)),
    )?;
    if let Some(r) = pre {
        let ord = ctx.get_field(r, 1).as_int().unwrap_or(0);
        if ord == 1 {
            return Ok(false);
        } // TERMINATE
        if ord == 2 {
            return Ok(true);
        } // SKIP_SUBTREE
    }
    if remaining_depth > 0 {
        if let Some((jar, entry)) = jarfs_decode(dir) {
            // jar-FS directory — children come from the archive listing, not the
            // host filesystem (std::fs::read_dir on the encoded sentinel string
            // would ENOENT and silently visit nothing, so e.g. JUnit5's
            // ClasspathScanner would "discover" an empty jar).
            // The ArchiveContainer indexer only visits directories.  Do not build
            // transient Java paths for every ignored archive file in that mode.
            for (child, is_dir) in jarfs_list_dir_classified(&jar, &entry) {
                let es = jarfs_encode(&jar, &child);
                if is_dir {
                    let epo = alloc_concurrent_synthetic(ctx, "java/nio/file/Path", 2);
                    let epo_pin = p98_pin(ctx, epo);
                    let s = ctx.create_string(&es);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    ctx.set_field(epo_now, 0, Value::Object(Some(s)));
                    if !p98_walk_dir(
                        ctx,
                        &es,
                        visitor_pin,
                        epo_pin,
                        skip_file_callbacks,
                        remaining_depth - 1,
                        follow_links,
                    )? {
                        return Ok(false);
                    }
                } else if !skip_file_callbacks {
                    let epo = alloc_concurrent_synthetic(ctx, "java/nio/file/Path", 2);
                    let epo_pin = p98_pin(ctx, epo);
                    let s = ctx.create_string(&es);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    ctx.set_field(epo_now, 0, Value::Object(Some(s)));
                    let fa = p98_alloc_basic_file_attributes(
                        ctx,
                        false,
                        jarfs_entry_size(&jar, &child).unwrap_or(0),
                    );
                    let visitor_now = p98_read_pin(ctx, visitor_pin);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    let vr = p98_invoke_file_visitor(
                    ctx,
                    visitor_now,
                    "visitFile",
                    "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    epo_now,
                    Value::Object(Some(fa)),
                )?;
                    if let Some(r) = vr {
                        if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
                            return Ok(false);
                        }
                    }
                }
            }
        } else if let Some((java_home, entry)) = jrtfs_decode(dir) {
            // jrt-FS directory — children come from the jimage listing.
            for (child, is_dir) in jrtfs_list_dir_classified(&java_home, &entry) {
                let es = jrtfs_encode(&java_home, &child);
                if is_dir {
                    let epo = alloc_concurrent_synthetic(ctx, "java/nio/file/Path", 2);
                    let epo_pin = p98_pin(ctx, epo);
                    let s = ctx.create_string(&es);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    ctx.set_field(epo_now, 0, Value::Object(Some(s)));
                    if !p98_walk_dir(
                        ctx,
                        &es,
                        visitor_pin,
                        epo_pin,
                        skip_file_callbacks,
                        remaining_depth - 1,
                        follow_links,
                    )? {
                        return Ok(false);
                    }
                } else if !skip_file_callbacks {
                    let epo = alloc_concurrent_synthetic(ctx, "java/nio/file/Path", 2);
                    let epo_pin = p98_pin(ctx, epo);
                    let s = ctx.create_string(&es);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    ctx.set_field(epo_now, 0, Value::Object(Some(s)));
                    let fa = p98_alloc_basic_file_attributes(
                        ctx,
                        false,
                        jrtfs_entry_size(&java_home, &child).unwrap_or(0),
                    );
                    let visitor_now = p98_read_pin(ctx, visitor_pin);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    let vr = p98_invoke_file_visitor(
                    ctx,
                    visitor_now,
                    "visitFile",
                    "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    epo_now,
                    Value::Object(Some(fa)),
                )?;
                    if let Some(r) = vr {
                        if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
                            return Ok(false);
                        }
                    }
                }
            }
        } else if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let ep = entry.path();
                let es = ep.to_string_lossy().to_string();
                // `entry.file_type()` is the NOFOLLOW answer (it comes straight
                // off the readdir record), so a symbolic link is a link here
                // whatever it points at. `Path::is_dir()` resolves it — which is
                // exactly the "descend unless FOLLOW_LINKS" decision the walk
                // has to make, so only ask it when following.
                let is_dir = entry
                    .file_type()
                    .map(|t| t.is_dir() || (follow_links && t.is_symlink() && ep.is_dir()))
                    .unwrap_or(false);
                // HOST path (not a virtual-FS sentinel): build it through
                // `p57_alloc_path` so the stored string is the `/`-canonical
                // internal form every other Path native expects. Setting field 0
                // directly left `entry.path()`'s host separators in place, so on
                // Windows a Path handed to a FileVisitor compared unequal to the
                // otherwise-identical Path the caller built, and `startsWith`/
                // `relativize`/`getNameCount` against it all disagreed.
                if is_dir {
                    let epo = p57_alloc_path(ctx, &es);
                    let epo_pin = p98_pin(ctx, epo);
                    if !p98_walk_dir(
                        ctx,
                        &es,
                        visitor_pin,
                        epo_pin,
                        skip_file_callbacks,
                        remaining_depth - 1,
                        follow_links,
                    )? {
                        return Ok(false);
                    }
                } else if !skip_file_callbacks {
                    let epo = p57_alloc_path(ctx, &es);
                    let epo_pin = p98_pin(ctx, epo);
                    let fa = p98_alloc_basic_file_attributes(
                        ctx,
                        false,
                        entry
                            .metadata()
                            .map(|metadata| metadata.len() as i64)
                            .unwrap_or(0),
                    );
                    let visitor_now = p98_read_pin(ctx, visitor_pin);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    let vr = p98_invoke_file_visitor(
                    ctx,
                    visitor_now,
                    "visitFile",
                    "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    epo_now,
                    Value::Object(Some(fa)),
                )?;
                    if let Some(r) = vr {
                        if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
                            return Ok(false);
                        }
                    }
                }
            }
        }
    }
    let visitor_now = p98_read_pin(ctx, visitor_pin);
    let dir_path_now = p98_read_pin(ctx, dir_path_pin);
    let post = p98_invoke_file_visitor(
        ctx,
        visitor_now,
        "postVisitDirectory",
        "(Ljava/nio/file/Path;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
        "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
        dir_path_now,
        Value::Object(None),
    )?;
    if let Some(r) = post {
        if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
            return Ok(false);
        }
    }
    Ok(true)
}

// =============================================================================
// java.nio.file.WatchService stubs
// =============================================================================

pub(crate) fn register_p66_watch_service(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);

    // NOTE — this function no longer registers `WatchService.close/poll/take`,
    // `Path.register`, or the `WatchKey` methods.
    //
    // It used to carry a second, competing WatchService implementation built
    // on a 2-field layout (dir_path=0, last_scan_time=1) that answered `poll`
    // by re-`stat`ing the directory and comparing mtimes. The owner of the
    // WatchService surface is `cratonvm-native-io`'s `register_watch_service`,
    // which is backed by a real platform watcher (`notify` → inotify /
    // ReadDirectoryChangesW / FSEvents), reports the actual changed entry
    // through `WatchEvent.context()`, and uses an incompatible 3-field
    // WatchService / 5-field WatchKey layout.
    //
    // Two implementations of the same triples resolved by last-write-wins, so
    // which one ran depended purely on registration order between arms — and
    // the two layouts are not interchangeable, so the losing side's objects
    // are garbage to the winning side's natives. That is exactly how the
    // Spring Boot `FileWatcher` failure arose (a placeholder `newWatchService`
    // displacing the real one). Only the `StandardWatchEventKinds` constants
    // stay here: they are plain named singletons, not a second implementation.

    // StandardWatchEventKinds
    let swek = "java/nio/file/StandardWatchEventKinds";
    r.register(
        swek,
        "ENTRY_CREATE",
        "Ljava/nio/file/WatchEvent$Kind;",
        |ctx, _args| {
            let s = ctx.create_string("ENTRY_CREATE");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        swek,
        "ENTRY_MODIFY",
        "Ljava/nio/file/WatchEvent$Kind;",
        |ctx, _args| {
            let s = ctx.create_string("ENTRY_MODIFY");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        swek,
        "ENTRY_DELETE",
        "Ljava/nio/file/WatchEvent$Kind;",
        |ctx, _args| {
            let s = ctx.create_string("ENTRY_DELETE");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        swek,
        "OVERFLOW",
        "Ljava/nio/file/WatchEvent$Kind;",
        |ctx, _args| {
            let s = ctx.create_string("OVERFLOW");
            Ok(Some(Value::Object(Some(s))))
        },
    );

    r.set_category(__prev_cat);
}

// =============================================================================
// java.nio.file.attribute extensions — PosixFilePermission, FileTime, UserPrincipal
// =============================================================================

/// Synthetic class backing `PosixFilePermissions.asFileAttribute`'s result.
/// Field 0 = the attribute name (`"posix:permissions"`), field 1 = the
/// `Set<PosixFilePermission>` value — the same two things the interface's
/// `name()`/`value()` return.
pub(crate) const FILE_ATTRIBUTE_CLASS: &str = "java/nio/file/attribute/FileAttribute";

/// Unix mode requested by a `[Ljava/nio/file/attribute/FileAttribute;` varargs
/// argument, i.e. by `PosixFilePermissions.asFileAttribute(...)`.
///
/// The real JDK applies this at creation time (the `mkdir(2)`/`open(2)` mode
/// argument); every `Files.create*` native here used to ignore the array
/// outright, so the created file/directory got the process umask instead of
/// the caller's requested mode. Returns `None` when the array is absent,
/// empty, or carries no `posix:permissions` attribute — callers then keep
/// their previous default behaviour.
pub(crate) fn posix_mode_from_file_attributes(
    ctx: &mut dyn NativeContext,
    arg: Option<&Value>,
) -> Option<u32> {
    let arr = match arg {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    let len = ctx.array_length(arr);
    for i in 0..len {
        let elem = match ctx.get_array_element(arr, i) {
            Value::Object(Some(e)) => e,
            _ => continue,
        };
        // Synthetic `asFileAttribute` result: read the two fields directly.
        // A real JDK `PosixFilePermissions$1` has no such layout, so fall
        // back to the interface methods for it — and check the object's width
        // first, since probing slots it does not have is an out-of-bounds read
        // the heap guard reports.
        let two_field_carrier = ctx.object_num_fields(elem) > 1;
        let mut name = match two_field_carrier.then(|| ctx.get_field(elem, 0)) {
            Some(Value::Object(Some(s))) => ctx.read_string(s),
            _ => None,
        };
        let mut value = if two_field_carrier {
            ctx.get_field(elem, 1)
        } else {
            Value::Object(None)
        };
        if name.as_deref() != Some("posix:permissions") {
            name = match ctx.invoke_virtual(elem, "name", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                _ => None,
            };
            if name.as_deref() != Some("posix:permissions") {
                continue;
            }
            value = match ctx.invoke_virtual(elem, "value", "()Ljava/lang/Object;", &[]) {
                Ok(Some(v)) => v,
                _ => continue,
            };
        }
        if let Value::Object(Some(set)) = value {
            return Some(posix_permission_bits_from_set(ctx, set));
        }
    }
    None
}

/// `std::fs::create_dir` with an explicit Unix mode when the caller asked for
/// one (atomic — the mode is passed to `mkdir(2)`, so the directory is never
/// briefly visible with wider permissions).
pub(crate) fn create_dir_with_mode(p: &str, mode: Option<u32>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        if let Some(m) = mode {
            b.mode(m);
        }
        return b.create(p);
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir(p)
    }
}

/// Best-effort `chmod` for paths that were already created (the recursive
/// `create_dir_all` and temp-file paths). No-op off Unix and when no mode was
/// requested.
pub(crate) fn apply_unix_mode(p: &str, mode: Option<u32>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(m) = mode {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (p, mode);
    }
}

/// Convert a `Set<PosixFilePermission>` (the 9 canonical singleton constants
/// from `posix_file_permission_stub_clinit`/the real enum) into a Unix
/// permission-bits mode (e.g. for `std::fs::Permissions::from_mode`). Walks
/// the same 9 constants `PosixFilePermissions.toString`/`fromString` use,
/// via `Set.contains` (no set-internals assumption — works for any real
/// `Set` implementation the caller passes in, not just our synthetic
/// `HashSet`).
pub(crate) fn posix_permission_bits_from_set(ctx: &mut dyn NativeContext, set: ObjectRef) -> u32 {
    const NAMES: [&str; 9] = [
        "OWNER_READ",
        "OWNER_WRITE",
        "OWNER_EXECUTE",
        "GROUP_READ",
        "GROUP_WRITE",
        "GROUP_EXECUTE",
        "OTHERS_READ",
        "OTHERS_WRITE",
        "OTHERS_EXECUTE",
    ];
    const BITS: [u32; 9] = [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ];
    let pfp = "java/nio/file/attribute/PosixFilePermission";
    let _ = ctx.ensure_class_initialized(pfp);
    let cid = ctx.class_id_by_name(pfp);
    let mut mode = 0u32;
    for i in 0..9 {
        let Some(c) = cid else { break };
        let Some(slot) = ctx.static_field_index_by_name(c, NAMES[i]) else {
            continue;
        };
        let constant = ctx.get_static_field(c, slot);
        if matches!(
            ctx.invoke_virtual(set, "contains", "(Ljava/lang/Object;)Z", &[constant]),
            Ok(Some(Value::Int(1)))
        ) {
            mode |= BITS[i];
        }
    }
    mode
}

pub(crate) fn posix_file_permission_stub_clinit(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    const P: &str = "java/nio/file/attribute/PosixFilePermission";
    if !ctx.is_class_synthetic_stub(P) {
        return Ok(None);
    }
    let Some(cid) = ctx.class_id_by_name(P) else {
        return Ok(None);
    };
    if let Some(slot) = ctx.static_field_index_by_name(cid, "OWNER_READ") {
        if matches!(ctx.get_static_field(cid, slot), Value::Object(Some(_))) {
            return Ok(None);
        }
    }
    const NAMES: &[&str] = &[
        "OWNER_READ",
        "OWNER_WRITE",
        "OWNER_EXECUTE",
        "GROUP_READ",
        "GROUP_WRITE",
        "GROUP_EXECUTE",
        "OTHERS_READ",
        "OTHERS_WRITE",
        "OTHERS_EXECUTE",
    ];
    let _ = ctx.ensure_class_initialized("java/lang/Enum");
    let ord_idx = ctx.resolve_field_index(P, "ordinal").unwrap_or(0);
    let name_idx = ctx.resolve_field_index(P, "name").unwrap_or(1);
    let nfields = ctx.class_num_total_fields(cid).max(2);
    for (ord, &name) in NAMES.iter().enumerate() {
        let obj = ctx.alloc_object(cid, nfields);
        ctx.set_field(obj, ord_idx, Value::Int(ord as i32));
        let name_obj = ctx.create_string(name);
        ctx.set_field(obj, name_idx, Value::Object(Some(name_obj)));
        ctx.set_static_field_by_name(P, name, Value::Object(Some(obj)));
    }
    Ok(None)
}

pub(crate) fn register_posix_file_permission_stub_clinit(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "java/nio/file/attribute/PosixFilePermission",
        "<clinit>",
        "()V",
        posix_file_permission_stub_clinit,
    );
    r.set_category(__prev_cat);
}

pub(crate) fn register_p70_file_attributes(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // FileTime — millis stored in the real `long value` field (by name) so
    // descriptor coercion preserves it; see `filetime_alloc`.
    let ft = "java/nio/file/attribute/FileTime";
    r.register(
        ft,
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            let millis = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let obj = filetime_alloc(ctx, millis);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ft,
        "from",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            let val = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            // Honor the TimeUnit (was ignored → SECONDS/etc. stored as raw
            // millis → wrong dates). Convert to millis via unit.toMillis(val).
            let millis = match args.get(1) {
                Some(Value::Object(Some(unit))) => {
                    match ctx.invoke_virtual(*unit, "toMillis", "(J)J", &[Value::Long(val)])? {
                        Some(Value::Long(m)) => m,
                        _ => val,
                    }
                }
                _ => val,
            };
            let obj = filetime_alloc(ctx, millis);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ft,
        "from",
        "(Ljava/time/Instant;)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            // Preserve the Instant's value (was dropped → stored 0, so every
            // FileTime.from(Instant) round-tripped to epoch 0). Read the real
            // java.time.Instant's named fields (seconds/nanos).
            let millis = match args.first() {
                Some(Value::Object(Some(inst))) => {
                    let secs = match ctx.get_field_by_name(*inst, "seconds") {
                        Value::Long(v) => v,
                        _ => 0,
                    };
                    let nanos = match ctx.get_field_by_name(*inst, "nanos") {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    secs.saturating_mul(1000) + (nanos as i64) / 1_000_000
                }
                _ => 0,
            };
            let obj = filetime_alloc(ctx, millis);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(ft, "toMillis", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(filetime_read_millis(ctx, this))))
    });
    r.register(ft, "toInstant", "()Ljava/time/Instant;", |ctx, args| {
        // Was hardcoded to null → every `FileTime.toInstant().getEpochSecond()`
        // NPE'd (Spring Boot buildpack tar-layer timestamps). Build a real
        // java.time.Instant from the stored millis via its real factory.
        let this = obj_arg(args, 0)?;
        let millis = filetime_read_millis(ctx, this);
        ctx.invoke(
            "java/time/Instant",
            "ofEpochMilli",
            "(J)Ljava/time/Instant;",
            &[Value::Long(millis)],
        )
    });
    r.register(
        ft,
        "compareTo",
        "(Ljava/nio/file/attribute/FileTime;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let a = filetime_read_millis(ctx, this);
            let b = filetime_read_millis(ctx, other);
            Ok(Some(Value::Int(a.cmp(&b) as i32)))
        },
    );
    r.register(ft, "toString", "()Ljava/lang/String;", |ctx, args| {
        // Was "{millis}ms"; real FileTime.toString is the ISO-8601 instant.
        let this = obj_arg(args, 0)?;
        let millis = filetime_read_millis(ctx, this);
        let inst = match ctx.invoke(
            "java/time/Instant",
            "ofEpochMilli",
            "(J)Ljava/time/Instant;",
            &[Value::Long(millis)],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                let s = ctx.create_string("");
                return Ok(Some(Value::Object(Some(s))));
            }
        };
        ctx.invoke_virtual(inst, "toString", "()Ljava/lang/String;", &[])
    });

    // PosixFilePermission enum
    let pfp = "java/nio/file/attribute/PosixFilePermission";
    // Register each individually (NativeCallback = fn ptr, no captures)
    r.register(
        pfp,
        "OWNER_READ",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "OWNER_READ",
                0,
            )
        },
    );
    r.register(
        pfp,
        "OWNER_WRITE",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "OWNER_WRITE",
                1,
            )
        },
    );
    r.register(
        pfp,
        "OWNER_EXECUTE",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "OWNER_EXECUTE",
                2,
            )
        },
    );
    r.register(
        pfp,
        "GROUP_READ",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "GROUP_READ",
                3,
            )
        },
    );
    r.register(
        pfp,
        "GROUP_WRITE",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "GROUP_WRITE",
                4,
            )
        },
    );
    r.register(
        pfp,
        "GROUP_EXECUTE",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "GROUP_EXECUTE",
                5,
            )
        },
    );
    r.register(
        pfp,
        "OTHERS_READ",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "OTHERS_READ",
                6,
            )
        },
    );
    r.register(
        pfp,
        "OTHERS_WRITE",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "OTHERS_WRITE",
                7,
            )
        },
    );
    r.register(
        pfp,
        "OTHERS_EXECUTE",
        "Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            p57_alloc_enum(
                ctx,
                "java/nio/file/attribute/PosixFilePermission",
                "OTHERS_EXECUTE",
                8,
            )
        },
    );

    r.register(
        pfp,
        "values",
        "()[Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 9);
            // Can't iterate/capture — just return the array (elements are null but array exists)
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // PosixFilePermissions utility
    let pfps = "java/nio/file/attribute/PosixFilePermissions";
    // The 9 PosixFilePermission constants in canonical "rwxrwxrwx" order, with
    // the rwx char expected at each position. The constants are stable singletons
    // (PosixFilePermission stub_clinit / real enum), so a HashSet of them works
    // with Set.contains(OWNER_READ) downstream.
    const PFP_NAMES: [&str; 9] = [
        "OWNER_READ",
        "OWNER_WRITE",
        "OWNER_EXECUTE",
        "GROUP_READ",
        "GROUP_WRITE",
        "GROUP_EXECUTE",
        "OTHERS_READ",
        "OTHERS_WRITE",
        "OTHERS_EXECUTE",
    ];
    const PFP_PAT: [char; 9] = ['r', 'w', 'x', 'r', 'w', 'x', 'r', 'w', 'x'];
    r.register(
        pfps,
        "toString",
        "(Ljava/util/Set;)Ljava/lang/String;",
        |ctx, args| {
            // Was a stub that always returned "rwxr-xr-x". Build the real
            // "rwxrwxrwx" string from the actual set membership.
            let set = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(Some(ctx.create_string("---------"))))),
            };
            let pfp = "java/nio/file/attribute/PosixFilePermission";
            let _ = ctx.ensure_class_initialized(pfp);
            let cid = ctx.class_id_by_name(pfp);
            let mut out = String::with_capacity(9);
            for i in 0..9 {
                let present = if let Some(c) = cid {
                    match ctx.static_field_index_by_name(c, PFP_NAMES[i]) {
                        Some(slot) => {
                            let constant = ctx.get_static_field(c, slot);
                            matches!(
                                ctx.invoke_virtual(
                                    set,
                                    "contains",
                                    "(Ljava/lang/Object;)Z",
                                    &[constant]
                                ),
                                Ok(Some(Value::Int(1)))
                            )
                        }
                        None => false,
                    }
                } else {
                    false
                };
                out.push(if present { PFP_PAT[i] } else { '-' });
            }
            Ok(Some(Value::Object(Some(ctx.create_string(&out)))))
        },
    );
    r.register(
        pfps,
        "fromString",
        "(Ljava/lang/String;)Ljava/util/Set;",
        |ctx, args| {
            // Was a stub returning an EMPTY set (ignored the input). Parse the
            // real "rwxrwxrwx" permission string into a HashSet of the singleton
            // PosixFilePermission constants.
            let perms = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let set = match ctx.new_object("java/util/HashSet") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let _ = ctx.invoke(
                "java/util/HashSet",
                "<init>",
                "()V",
                &[Value::Object(Some(set))],
            );
            let chars: Vec<char> = perms.chars().collect();
            let pfp = "java/nio/file/attribute/PosixFilePermission";
            // Pin across the clinit / add() invokes below — a moving young GC
            // there would relocate the fresh set (native stale-local family).
            let set_pin = ctx.pin_native_root(set);
            let _ = ctx.ensure_class_initialized(pfp);
            let cid = ctx.class_id_by_name(pfp);
            for i in 0..9 {
                if chars.get(i).copied() == Some(PFP_PAT[i]) {
                    if let Some(c) = cid {
                        if let Some(slot) = ctx.static_field_index_by_name(c, PFP_NAMES[i]) {
                            let constant = ctx.get_static_field(c, slot);
                            if matches!(constant, Value::Object(Some(_))) {
                                let set = ctx.read_native_pin(set_pin, set);
                                let _ = ctx.invoke_virtual(
                                    set,
                                    "add",
                                    "(Ljava/lang/Object;)Z",
                                    &[constant],
                                );
                            }
                        }
                    }
                }
            }
            let set = ctx.read_native_pin(set_pin, set);
            ctx.unpin_native_roots(set_pin);
            Ok(Some(Value::Object(Some(set))))
        },
    );
    // Bridges NSM when the method is missing from the resolved class table.
    // This native SHADOWS the real JDK's anonymous-class implementation even
    // in real-JDK mode, so returning `null` here (the previous behaviour) was
    // not a harmless fallback: it silently erased the requested mode from
    // every `Files.createDirectory(dir, PosixFilePermissions
    // .asFileAttribute(<700>))` call, which then created the directory with
    // the process umask (0755). Spring Boot's `ApplicationTemp` rejected its
    // OWN temp directory on the next call ("Existing directory ... does not
    // have the permissions [OWNER_READ, OWNER_WRITE, OWNER_EXECUTE]") —
    // `AbstractServletWebServerFactoryTests#persistSession` /
    // `#getValidSessionStoreWhenSessionStoreNotSet`. Hand back a two-field
    // synthetic carrier instead; `posix_mode_from_file_attributes` reads it,
    // and `name()`/`value()` below cover anyone who calls the interface.
    r.register(
        pfps,
        "asFileAttribute",
        "(Ljava/util/Set;)Ljava/nio/file/attribute/FileAttribute;",
        |ctx, args| {
            let set = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Pin across the alloc/create_string below — a moving young GC
            // there would relocate them (native stale-local family).
            let set_pin = ctx.pin_native_root(set);
            let fa = alloc_concurrent_synthetic(ctx, FILE_ATTRIBUTE_CLASS, 2);
            let fa_pin = ctx.pin_native_root(fa);
            let name = ctx.create_string("posix:permissions");
            let fa = ctx.read_native_pin(fa_pin, fa);
            let set = ctx.read_native_pin(set_pin, set);
            ctx.unpin_native_roots(set_pin);
            ctx.set_field(fa, 0, Value::Object(Some(name)));
            ctx.set_field(fa, 1, Value::Object(Some(set)));
            Ok(Some(Value::Object(Some(fa))))
        },
    );
    r.register(
        FILE_ATTRIBUTE_CLASS,
        "name",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        FILE_ATTRIBUTE_CLASS,
        "value",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    register_posix_file_permission_stub_clinit(r);
    r.set_category(__prev_cat);
}

// =============================================================================
// Files stream/IO bridge
// =============================================================================

pub(crate) fn register_p71_files_bridge(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let f = "java/nio/file/Files";

    // Files.newInputStream — a lazy stream over the file. NB: this is the
    // THIRD registration of this exact (class, name, descriptor); registration
    // is last-writer-wins, so whichever of them runs last is the one that
    // dispatches. They all route to the same helper now, which is why that no
    // longer matters.
    r.register(
        f,
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
        // args[0] = the Path (static method).
        |ctx, args| fsp_new_input_stream(ctx, args, 0),
    );

    r.register(
        f,
        "readAllBytes",
        "(Ljava/nio/file/Path;)[B",
        |ctx, args| {
            let p = p57_read_path(ctx, obj_arg(args, 0)?);
            match std::fs::read(&p) {
                Ok(bytes) => {
                    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
                    for (i, &b) in bytes.iter().enumerate() {
                        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                    }
                    Ok(Some(Value::Object(Some(arr))))
                }
                // NIO contract: missing file → NoSuchFileException, not a bare
                // IOException/IllegalStateException (see newByteChannel above).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p))
                }
                Err(e) => Err(RuntimeError::IllegalStateException {
                    message: format!("IOException: {}", e),
                }
                .into()),
            }
        },
    );
    r.register(
        f,
        "write",
        "(Ljava/nio/file/Path;[B[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        files_write_bytes_impl,
    );
    // Both `Files.copy` stream overloads returned 0 without moving a single
    // byte. The caller was told "copied 0 bytes" — a legal-looking answer for
    // an empty source — so an unpacked resource, a saved upload, or a streamed
    // download silently produced nothing, and no exception ever surfaced. They
    // now perform the copy and report the real byte count.
    r.register(
        f,
        "copy",
        "(Ljava/io/InputStream;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)J",
        |ctx, args| {
            let src = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Files.copy: source stream is null".into()),
                    }
                    .into())
                }
            };
            let target = obj_arg(args, 1)?;
            let replace = copy_options_replace_existing(ctx, args.get(2));
            // Pin `target` across the drain and the option probe: both re-enter
            // Java and a moving young GC there would relocate it (native
            // stale-local family).
            let target_pin = ctx.pin_native_root(target);
            let bytes = zip_streams::drain_input_stream_bulk(ctx, src);
            let target = ctx.read_native_pin(target_pin, target);
            let p = p57_read_path(ctx, target);
            ctx.unpin_native_roots(target_pin);
            // NIO contract: without REPLACE_EXISTING an existing target is a
            // FileAlreadyExistsException, not an overwrite.
            if !replace && std::fs::symlink_metadata(&p).is_ok() {
                return Err(p57_file_already_exists(ctx, &p));
            }
            match std::fs::write(&p, &bytes) {
                Ok(()) => Ok(Some(Value::Long(bytes.len() as i64))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p))
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        },
    );
    r.register(
        f,
        "copy",
        "(Ljava/nio/file/Path;Ljava/io/OutputStream;)J",
        |ctx, args| {
            let src = obj_arg(args, 0)?;
            let out = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Files.copy: target stream is null".into()),
                    }
                    .into())
                }
            };
            let p = p57_read_path(ctx, src);
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(p57_no_such_file(ctx, &p))
                }
                Err(e) => return Err(p57_io_error(&e)),
            };
            // Pin the sink across the array alloc, then hand the whole payload
            // over in ONE bulk `write([BII)` rather than a per-byte loop.
            let out_pin = ctx.pin_native_root(out);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            ctx.write_byte_array_from(arr, 0, &bytes);
            let out = ctx.read_native_pin(out_pin, out);
            let res = ctx.invoke_virtual(
                out,
                "write",
                "([BII)V",
                &[
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Int(bytes.len() as i32),
                ],
            );
            ctx.unpin_native_roots(out_pin);
            res?;
            Ok(Some(Value::Long(bytes.len() as i64)))
        },
    );
    // `Files.readAttributes(path, "view:attrs", options)` — the name-keyed form.
    //
    // This used to hand back an EMPTY HashMap for every call, which is the
    // shape of answer that is worse than an exception: the contract is that a
    // requested attribute is present or the call throws
    // (`IllegalArgumentException` for an unknown name,
    // `UnsupportedOperationException` for an unknown view), so every caller
    // dereferences `map.get(name)` unconditionally and got `null`.
    // `Files.getAttribute` is a thin wrapper over exactly this and returned
    // null for every attribute of every file.
    //
    // Args (static): `[path, attributes, options]`.
    r.register(
        f,
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/util/Map;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let spec = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let nofollow =
                matches!(args.get(2), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
            read_named_attributes(ctx, path_obj, &spec, nofollow)
        },
    );
    // `Files.getAttribute(path, "view:name", options)` — one attribute, by name.
    //
    // The real body is `readAttributes(path, name, options).get(name)`; register
    // it explicitly so synthetic-JDK mode (where there is no `Files` bytecode to
    // run) answers the same as real-JDK mode rather than falling through to a
    // missing method.
    r.register(
        f,
        "getAttribute",
        "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/lang/Object;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let spec = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let nofollow =
                matches!(args.get(2), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
            let (_, attr) = split_attribute_spec(&spec);
            // `getAttribute` takes a SINGLE name; `*` and comma lists are only
            // legal in `readAttributes`.
            if attr == "*" || attr.contains(',') {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("'{spec}' not recognized"),
                }
                .into());
            }
            let map = read_named_attributes(ctx, path_obj, &spec, nofollow)?;
            let map_obj = match map {
                Some(Value::Object(Some(m))) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            let map_pin = ctx.pin_native_root(map_obj);
            let key = ctx.create_string(attr);
            let map_obj = ctx.read_native_pin(map_pin, map_obj);
            let got = ctx.invoke_virtual(
                map_obj,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(key))],
            );
            ctx.unpin_native_roots(map_pin);
            Ok(Some(got?.unwrap_or(Value::Object(None))))
        },
    );
    // Was null. `Files.getFileStore` never returns null in the real JDK, so
    // every caller dereferences the result unconditionally (ES's `ESFileStore`
    // wrapper does) — a guaranteed NPE. Hand back the same synthetic
    // `FileStore` the `FileSystemProvider.getFileStore` native above builds.
    r.register(
        f,
        "getFileStore",
        "(Ljava/nio/file/Path;)Ljava/nio/file/FileStore;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            // NIO contract: the path must exist.
            if std::fs::symlink_metadata(&p).is_err() {
                return Err(p57_no_such_file(ctx, &p));
            }
            Ok(Some(Value::Object(Some(p57_alloc_file_store(ctx, &p)))))
        },
    );
    // Was `null`. `Files.getOwner` is specified to return a principal or throw
    // (`UnsupportedOperationException` when the path's provider has no
    // `FileOwnerAttributeView`, `IOException` on failure) — never null — so
    // every caller dereferences it. Same delegation as
    // `PosixFileAttributeView.getOwner`: the real
    // `UnixFileAttributes.owner()` bytecode turns the `st_uid` we record into a
    // proper `UnixUserPrincipal` via the wired `getpwuid` native.
    r.register(
        f,
        "getOwner",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/UserPrincipal;",
        |ctx, args| {
            let path_value = args.first().copied().unwrap_or(Value::Object(None));
            nio_owner_principal(ctx, path_value)
        },
    );
    r.register(
        f,
        "setOwner",
        "(Ljava/nio/file/Path;Ljava/nio/file/attribute/UserPrincipal;)Ljava/nio/file/Path;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    // Accessors for the carrier `nio_owner_principal` returns on Windows.
    // `getName()` is the whole of `UserPrincipal`; `toString`/`equals`/`hashCode`
    // mirror `sun.nio.fs.WindowsUserPrincipals$User`, which compares on the SID
    // string (two principals for the same SID are equal even when the account
    // name renders differently) and prints `account (type)`.
    {
        let wup = "sun/nio/fs/WindowsUserPrincipals$User";
        r.register(wup, "getName", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "accountName")))
        });
        r.register(wup, "toString", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match ctx.get_field_by_name(this, "accountName") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // `SidType` values from Win32 `SID_NAME_USE`, in declaration order.
            let kind = match ctx.get_field_by_name(this, "sidType") {
                Value::Int(1) => "USER",
                Value::Int(2) => "GROUP",
                Value::Int(3) => "DOMAIN",
                Value::Int(4) => "ALIAS",
                Value::Int(5) => "WELL_KNOWN_GROUP",
                Value::Int(6) => "DELETED_ACCOUNT",
                Value::Int(7) => "INVALID",
                Value::Int(9) => "COMPUTER",
                _ => "UNKNOWN",
            };
            let s = ctx.create_string(&format!("{name} ({kind})"));
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(wup, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let Some(Value::Object(Some(other))) = args.get(1).copied() else {
                return Ok(Some(Value::Int(0)));
            };
            if ctx.class_id_of_object(other) != ctx.class_id_of_object(this) {
                return Ok(Some(Value::Int(0)));
            }
            let read = |ctx: &mut dyn NativeContext, o| match ctx.get_field_by_name(o, "sidString") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let a = read(ctx, this);
            let b = read(ctx, other);
            Ok(Some(Value::Int(i32::from(!a.is_empty() && a == b))))
        });
        r.register(wup, "hashCode", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = match ctx.get_field_by_name(this, "sidString") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // `String.hashCode()`, so the value matches what the real carrier
            // (which hashes the same field) would produce.
            let mut hash: i32 = 0;
            for ch in sid.encode_utf16() {
                hash = hash.wrapping_mul(31).wrapping_add(i32::from(ch));
            }
            Ok(Some(Value::Int(hash)))
        });
    }
    // These three used to return `args[0]` — the link path — without creating
    // or reading anything, i.e. they claimed success for a link that was never
    // made. `Files.createSymbolicLink`/`createLink`/`readSymbolicLink` all have
    // real bytecode that delegates to the provider, so these registrations lose
    // to it and the lie was never observed; the provider natives registered on
    // `java/nio/file/spi/FileSystemProvider` are what actually run. They are
    // kept, and made real, so that any dispatch path that DOES prefer a
    // registration over bytecode gets the same answer as the provider.
    r.register(
        f,
        "createLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let link_obj = obj_arg(args, 0)?;
            let existing = obj_arg(args, 1)?;
            // `p57_read_path` can call back into `Path.toString()` bytecode for
            // a foreign Path implementation, so it is a GC point: pin the link
            // we hand back across both reads (native stale-local family).
            let link_pin = ctx.pin_native_root(link_obj);
            let link = p57_read_path(ctx, link_obj);
            let existing = p57_read_path(ctx, existing);
            let result = p57_create_hard_link(ctx, &link, &existing);
            let link_obj = ctx.read_native_pin(link_pin, link_obj);
            ctx.unpin_native_roots(link_pin);
            result?;
            Ok(Some(Value::Object(Some(link_obj))))
        },
    );
    r.register(f, "createSymbolicLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        |ctx, args| {
            let link_obj = obj_arg(args, 0)?;
            let target = obj_arg(args, 1)?;
            let link_pin = ctx.pin_native_root(link_obj);
            let link = p57_read_path(ctx, link_obj);
            let target = p57_read_path(ctx, target);
            let result = p57_create_symbolic_link(ctx, &link, &target);
            let link_obj = ctx.read_native_pin(link_pin, link_obj);
            ctx.unpin_native_roots(link_pin);
            result?;
            Ok(Some(Value::Object(Some(link_obj))))
        });
    r.register(
        f,
        "readSymbolicLink",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, path_obj);
            p57_read_symbolic_link(ctx, &p)
        },
    );
    r.set_category(__prev_cat);
}
