#[allow(unused_imports)]
use super::*;

/// Resolve `other` against `base` as per `Path.resolve()` semantics:
/// - If `other` is absolute, return `other`.
/// - If `other` is empty, return `base`.
/// - Otherwise, join `base` + separator + `other`.
pub fn p57_resolve_paths(base: &str, other: &str) -> String {
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



/// One `more` part of JDK 25's `UnixFileSystem.getPath(first, more...)` /
/// `WindowsFileSystem.getPath` (the body behind `Path.of` and `Paths.get`):
///
/// ```text
/// for (String segment : more)
///     if (!segment.isEmpty()) { if (sb.length() > 0) sb.append('/'); sb.append(segment); }
/// ```
///
/// then the path parser collapses the separator run that join can create.
/// The parts are JOINED, never resolved: `Path.of("a", "/b")` is `a/b`
/// (`a\b` on Windows), where the resolve fold answered `/b`. The run is
/// collapsed here at the join point only -- the separators inside a part are
/// the allocator's business, as they are for a one-argument `Path.of`. A
/// virtual (jar / jrt) accumulator keeps the resolve rule of its own
/// namespace.
/// `WindowsFileSystem.getPath(first, more)`'s LITERAL join: a non-empty
/// segment is appended after a `\` when the text so far is non-empty, nothing
/// trimmed (round 15 wave 7, lane drainA; N2b of `r15w6-jni6-review-findings`).
/// The parser sees this text, so a separator run the join creates at the start
/// is a UNC prefix: `Path.of("/", "x")` is `/\x`, refused with `UNC path is
/// missing sharename: /\x`. [`p57_get_path_join`] collapses the run first and
/// answered `\x`. Built only on Windows (`cfg!(windows)` at the callers).
pub fn p57_windows_literal_join(literal: &mut String, part: &str) {
    if part.is_empty() {
        return;
    }
    if !literal.is_empty() {
        literal.push('\\');
    }
    literal.push_str(part);
}

/// The parser's refusal of a literal join ([`p57_windows_literal_join`]) as
/// `(input, reason, index)`, or `None` to keep the collapsed join (which the
/// callers then check and store as before). Jar / vfs sentinels are never
/// Windows path syntax. Always `None` off Windows.
pub fn p57_windows_join_refusal(literal: &str) -> Option<(String, String, i32)> {
    #[cfg(windows)]
    {
        if jarfs_decode(literal).is_some() || vfs_decode(literal).is_some() {
            return None;
        }
        p57_windows_parser_refusal(literal).map(|r| (r.input, r.reason, r.index))
    }
    #[cfg(not(windows))]
    {
        let _ = literal;
        None
    }
}

pub fn p57_get_path_join(acc: &mut String, part: &str) {
    if part.is_empty() {
        return;
    }
    if acc.is_empty() {
        acc.push_str(part);
        return;
    }
    if vfs_decode(acc.as_str()).is_some() {
        let resolved = p57_resolve_paths(acc.as_str(), part);
        *acc = resolved;
        return;
    }
    let is_sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
    if !acc.ends_with(is_sep) {
        acc.push('/');
    }
    acc.push_str(part.trim_start_matches(is_sep));
}


/// Extract parent directory from a path string. Returns empty string for root-only paths.
pub fn p57_parent_of(path: &str) -> String {
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

/// Allocate the synthetic default-shaped FileSystem object every jar-mounted
/// FileSystem builds on ([`p57_alloc_jar_filesystem`]) -- the shape
/// [`p57_alloc_default_filesystem`] itself used unconditionally before the
/// 2026-09-17 carrier fix, and still uses when no concrete layout resolves.
/// FileSystem = 4-field synthetic (field 0 = separator String).
///
/// `#[track_caller]` (nio21 wave 11, lane L1): the object is an instance of
/// the ABSTRACT `java.nio.file.FileSystem`, which the JVMS 6.5
/// uninstantiable-receiver census reports by the location that asked for it.
/// Through this attribute that location is the CALLER -- the default
/// filesystem's arms in [`p57_alloc_default_filesystem`] or the jar mount in
/// [`p57_alloc_jar_filesystem`] -- instead of this one line for all of them,
/// which is what made the Spring Boot sample's `--compatible` row
/// (`docs/feature-designs/native-dispatch-architecture.md`)
/// ambiguous between the two.
#[track_caller]
pub fn p57_alloc_default_filesystem_synthetic(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    // Field 0 = separator; field 1 (P57_FS_JAR_FIELD) = mounted-JAR path (or
    // null); field 2 (P57_FS_JRT_FIELD) = mounted runtime-image java.home (or
    // null); field 3 (P57_FS_PROVIDER_FIELD) = the provider singleton for this
    // FS (or null until first `provider()` call). The jrt field exists on every
    // FS object so the jrt-aware FileSystem.getPath/getRootDirectories natives
    // can read it unconditionally.
    let fs = try_alloc_concurrent_synthetic(ctx, "java/nio/file/FileSystem", P57_FS_SLOTS)?;
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh FileSystem (native stale-local family).
    let fs_pin = ctx.pin_native_root(fs);
    let sep = if cfg!(windows) { "\\" } else { "/" };
    let s = ctx.create_string(sep);
    let fs = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs, 0, Value::Object(Some(s)));
    ctx.unpin_native_roots(fs_pin);
    Ok(fs)
}

/// The attribute views `getFileAttributeView` mints, by interface, and the
/// JDK's own concrete classes that stand for them (JVMS 6.5: a native may not
/// hand back an instance of the interface). The first of a list the image has
/// wins; an image with none keeps the interface stamp.
pub const P57_VIEW_IMPLS: &[(&str, &[&str])] = &[
    (
        "java/nio/file/attribute/BasicFileAttributeView",
        &[
            "sun/nio/fs/UnixFileAttributeViews$Basic",
            "sun/nio/fs/WindowsFileAttributeViews$Basic",
        ],
    ),
    (
        "java/nio/file/attribute/DosFileAttributeView",
        &["sun/nio/fs/WindowsFileAttributeViews$Dos"],
    ),
    (
        "java/nio/file/attribute/PosixFileAttributeView",
        &["sun/nio/fs/UnixFileAttributeViews$Posix"],
    ),
];

/// The class a minted attribute view wears: [`P57_VIEW_IMPLS`]' first
/// instantiable candidate for `iface`, else `iface` itself.
pub fn p57_concrete_view_class(ctx: &mut dyn NativeContext, iface: &'static str) -> &'static str {
    for (name, impls) in P57_VIEW_IMPLS {
        if *name == iface {
            return cratonvm_native_api::instantiable::first_instantiable(ctx, impls)
                .map_or(iface, |(class, _)| class);
        }
    }
    iface
}

/// Allocate THE default (file-scheme) FileSystem singleton -- concretely
/// stamped as the real platform class (`sun.nio.fs.WindowsFileSystem` on
/// Windows) when `cratonvm_native_api::filesystem_layout` resolves a layout
/// for it, else the pre-2026-09-17 synthetic shape, unchanged.
///
/// **Only the file-scheme default is minted concrete.**
/// [`p57_alloc_jar_filesystem`]/[`p57_alloc_jrt_filesystem`] deliberately
/// build on [`p57_alloc_default_filesystem_synthetic`] instead of this
/// function: their real counterparts (`jdk.nio.zipfs.ZipFileSystem`,
/// `jdk.internal.jrtfs.JrtFileSystem`) are unrelated classes with unrelated
/// field layouts this VM does not model -- see
/// `cratonvm_native_api::filesystem_layout`'s own doc for the full account,
/// including why every `P57_FS_JAR_FIELD`/`P57_FS_JRT_FIELD` reader in this
/// file is now width-guarded (`p57_fs_is_synthetic_shaped`) against a
/// genuinely concrete receiver landing here too.
pub fn p57_alloc_default_filesystem(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    // Resolve the Path slot map NOW. Until wave 16 every `Path` was minted by a
    // native allocator, which resolved it as a side effect; once
    // `WindowsFileSystem.getPath` is retired the REAL `WindowsPath.<init>` builds
    // them and no allocator runs, so every native READER (`path_slot_string`,
    // which holds only `&dyn NativeContext` and cannot resolve) fell back to the
    // pre-2026-09-10 layout and read `fs` where the path String is -- an empty
    // `toString()` for every path. The default FileSystem is the one object
    // every Path needs first, so it is the earliest `&mut` site.
    let _ = p57_path_slots_init(ctx);
    match cratonvm_native_api::filesystem_layout::resolve(ctx) {
        Some(slots) => p57_alloc_default_filesystem_concrete(ctx, slots),
        // `--real-jdk` on a Unix host: real `UnixPath` bytecode runs against the
        // Path carrier and calls `fs.needToResolveAgainstDefaultDirectory()`,
        // which the abstract-stamped synthetic FileSystem does not have
        // (`NoSuchMethodError` at `UnixPath.getByteArrayForSysCalls`, the shape
        // that failed `ResourceTests` -- see the Windows account above).
        //
        // nio21 wave 11, lane L1: `--compatible` on a Unix host still answers
        // the abstract synthetic stand-in, an instance of `java.nio.file.
        // FileSystem` that `new` could never produce (JVMS 6.5; the Spring Boot
        // sample's census row). `CRATONVM_COMPAT_UNIX_JDK_DEFAULT_FS` takes the
        // same real `LinuxFileSystem` there (Windows' concrete default is
        // minted in both modes above). Opt-in in wave 11; default ON since
        // nio21 wave 14 (lane L3) after the w12 host run with it `=1`.
        None if !cfg!(windows) && (ctx.is_jdk_only() || true) => {
            match p57_alloc_default_filesystem_unix(ctx)? {
                Some(fs) => Ok(fs),
                None => p57_alloc_default_filesystem_synthetic(ctx),
            }
        }
        None => p57_alloc_default_filesystem_synthetic(ctx),
    }
}



/// Mint the default FileSystem as the REAL `sun.nio.fs.LinuxFileSystem`, by
/// running its own constructor over this VM's concrete provider.
///
/// The constructor is the JDK's own: it stores the provider, parses
/// `user.dir` into `defaultDirectory`, reads `sun.nio.fs.chdirAllowed` into
/// `needToResolveAgainstDefaultDirectory` and builds `rootDirectory`. Nothing
/// here re-implements those, so the byte-encoded `defaultDirectory` (a
/// `byte[]`, unlike Windows' `String`) is exactly what HotSpot would write.
///
/// `Ok(None)` when the real class cannot be instantiated (a synthetic-JDK
/// image), so the caller keeps the synthetic shape.
pub fn p57_alloc_default_filesystem_unix(
    ctx: &mut dyn NativeContext,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    const CLS: &str = "sun/nio/fs/LinuxFileSystem";
    let is_stub = ctx.is_class_synthetic_stub(CLS);
    let init_result = if is_stub {
        None
    } else {
        Some(ctx.ensure_class_initialized(CLS))
    };
    if is_stub || init_result.as_ref().is_some_and(Result::is_err) {
        // Should not happen with a real JDK image under `--real-jdk` --
        // `sun/nio/fs/LinuxFileSystem` is an ordinary bundled class with no
        // known circular-init dependency (unlike e.g.
        // `PrimitiveClassDescImpl`/`ConstantDescs`, which vm_util.rs special-
        // cases). Loud and always-on (not debug-gated) because the caller
        // falls back to an abstract synthetic stand-in that cannot serve a
        // real `UnixPath.needToResolveAgainstDefaultDirectory()` call, and
        // `p57_default_filesystem_singleton` keeps whatever it caches first.
        // Never observed: the one `NoSuchMethodError` cluster of that shape
        // came from the `jrt:` filesystem instead
        // (`fs-cluster-needtoresolveagainstdefaultdirectory-FIXED-20260923.md`).
        tracing::warn!(
            is_synthetic_stub = is_stub,
            init_error = init_result.is_some_and(|r| r.is_err()),
            "default FileSystem: sun/nio/fs/LinuxFileSystem could not be minted concrete; \
             falling back to the abstract synthetic stand-in"
        );
        return Ok(None);
    }
    // SBR wave 5, lane fs2: the default FileSystem IS the one the JDK's own
    // `DefaultFileSystemProvider.<clinit>` builds (`INSTANCE = new
    // LinuxFileSystemProvider()`, whose constructor builds `theFileSystem`).
    // Minting a provider and a filesystem of our own beside it made two
    // defaults the moment real bytecode touched `DefaultFileSystemProvider`
    // -- which is why its `instance`/`theFileSystem` shadows could not
    // retire. With the singleton taken FROM the JDK, the natives
    // (`FileSystems.getDefault`, `installedProviders`, `fs.provider()`) and
    // the real doors answer one object.
    if let Some(fs) = p57_default_filesystem_from_jdk(ctx) {
        return Ok(Some(fs));
    }
    let provider = p57_alloc_provider(ctx, "file")?;
    let provider_pin = ctx.pin_native_root(provider);
    let user_dir = ctx
        .get_system_property("user.dir")
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "/".to_string());
    let dir = ctx.create_string(&user_dir);
    let provider = ctx.read_native_pin(provider_pin, provider);
    let built = ctx.new_object_initialized(
        CLS,
        "(Lsun/nio/fs/UnixFileSystemProvider;Ljava/lang/String;)V",
        &[Value::Object(Some(provider)), Value::Object(Some(dir))],
    );
    let result = match built? {
        Some(Value::Object(Some(fs))) => {
            // The real constructor set `fs.provider`; it does not (and on
            // real JDK cannot, since it runs AFTER the provider's own
            // constructor already built) set the provider's `theFileSystem`
            // back-link — see `p57_link_provider_to_filesystem`'s doc.
            let provider = ctx.read_native_pin(provider_pin, provider);
            p57_link_provider_to_filesystem(ctx, provider, fs);
            Some(fs)        }
        _ => None,
    };
    ctx.unpin_native_roots(provider_pin);
    Ok(result)
}

/// `DefaultFileSystemProvider.INSTANCE.theFileSystem`, running the class's
/// real `<clinit>` if it has not run -- the JDK's own default provider and
/// filesystem (SBR wave 5, lane fs2; `--real-jdk` on Linux, the only caller).
///
/// `None` (the caller keeps its own mint) when the class is a stub, its
/// initialisation fails, or `INSTANCE` is still null -- the last only when
/// this is re-entered from inside that same `<clinit>` on this thread, which
/// is logged because the two defaults would then differ.
pub fn p57_default_filesystem_from_jdk(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    const DFSP: &str = "sun/nio/fs/DefaultFileSystemProvider";
    if ctx.is_class_synthetic_stub(DFSP) {
        return None;
    }
    if ctx.ensure_class_initialized(DFSP).is_err() {
        tracing::warn!("default FileSystem: {DFSP}.<clinit> failed; minting the VM's own");
        return None;
    }
    let cid = ctx.class_id_by_name(DFSP)?;
    let idx = ctx.static_field_index_by_name(cid, "INSTANCE")?;
    let provider = match ctx.get_static_field(cid, idx) {
        Value::Object(Some(p)) => p,
        _ => {
            tracing::warn!(
                "default FileSystem: {DFSP}.INSTANCE is null (re-entered from its own <clinit>); \
                 minting the VM's own"
            );
            return None;
        }
    };
    let field =
        ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(provider), "theFileSystem")?;
    match ctx.get_field(provider, field) {
        Value::Object(Some(fs)) => Some(fs),
        _ => None,
    }
}

/// The concrete-mint half of [`p57_alloc_default_filesystem`].
///
/// `defaultDirectory`/`defaultRoot` are computed the SAME way HotSpot's own
/// `WindowsFileSystemProvider` constructor computes them: by calling the
/// real, already-fully-functional `sun.nio.fs.WindowsPathParser.parse`
/// (package-private, non-native — this VM's natives bypass Java access
/// control) on `user.dir`, not by reimplementing Windows drive/UNC-root
/// parsing here. Byte-identical to what HotSpot would have written, and
/// immune to whatever separator convention this VM's own synthetic `Path`
/// carrier happens to store internally.
pub fn p57_alloc_default_filesystem_concrete(
    ctx: &mut dyn NativeContext,
    slots: cratonvm_native_api::filesystem_layout::FileSystemSlots,
) -> Result<ObjectRef, MethodCallFailed> {
    // Mint the provider FIRST and pin it across the FileSystem mint below —
    // both allocate, so either can move the other (native stale-local
    // family). Written once here; every subsequent real `.provider()` call
    // reads this same field back (see `p57_fs_provider`'s own doc on why
    // that identity matters — `UnixDomainSockets.generateTempName` compares
    // it with a raw `!=`).
    let provider = p57_alloc_provider(ctx, "file")?;
    let provider_pin = ctx.pin_native_root(provider);

    let fs = cratonvm_native_io::concrete_receiver::alloc_concrete(
        ctx,
        &[cratonvm_native_api::filesystem_layout::impl_class()],
        "java/nio/file/FileSystem",
        0,
    )
    .obj;
    let fs_pin = ctx.pin_native_root(fs);

    let provider = ctx.read_native_pin(provider_pin, provider);
    let fs2 = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs2, slots.provider, Value::Object(Some(provider)));
    p57_link_provider_to_filesystem(ctx, provider, fs2);
    // Both pins stay until the end: `unpin_native_roots(base)` TRUNCATES the
    // pin stack at `base`, so releasing `provider_pin` here also dropped
    // `fs_pin` (pushed after it), and every later `read_native_pin(fs_pin,
    // fs)` answered its stale fallback. Under ZGC stress that stored
    // `defaultDirectory` / `defaultRoot` into a vacated span
    // (`common-w20v-zgc-mark-skips-wild-child-pointers-under-nio-selector-stress`).

    let user_dir = ctx
        .get_system_property("user.dir")
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".to_string());
    let user_dir_str = ctx.create_string(&user_dir);
    let parsed = ctx.invoke(
        "sun/nio/fs/WindowsPathParser",
        "parse",
        "(Ljava/lang/String;)Lsun/nio/fs/WindowsPathParser$Result;",
        &[Value::Object(Some(user_dir_str))],
    );
    let result = match parsed {
        Ok(Some(Value::Object(Some(r)))) => r,
        _ => {
            ctx.unpin_native_roots(provider_pin);
            return Err(RuntimeError::IllegalStateException {
                message: "WindowsPathParser.parse(user.dir) returned no Result".into(),
            }
            .into());
        }
    };
    let result_pin = ctx.pin_native_root(result);
    let dir = ctx.invoke_virtual(result, "path", "()Ljava/lang/String;", &[]);
    let dir_str = match dir {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or(user_dir.clone()),
        _ => user_dir.clone(),
    };
    let result = ctx.read_native_pin(result_pin, result);
    let root = ctx.invoke_virtual(result, "root", "()Ljava/lang/String;", &[]);
    let root_str = match root {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    ctx.unpin_native_roots(result_pin);

    let dir_obj = ctx.create_string(&dir_str);
    let fs2 = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs2, slots.default_directory, Value::Object(Some(dir_obj)));
    let root_obj = ctx.create_string(&root_str);
    let fs2 = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs2, slots.default_root, Value::Object(Some(root_obj)));

    let fs2 = ctx.read_native_pin(fs_pin, fs);
    ctx.unpin_native_roots(provider_pin);
    Ok(fs2)
}

pub fn p57_alloc_jar_filesystem(
    ctx: &mut dyn NativeContext,
    jar_path: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let fs = p57_alloc_default_filesystem_synthetic(ctx)?;
    if jar_path.is_empty() {
        return Ok(fs);
    }
    // Pin across the create_string below: a moving young GC there would
    // relocate the fresh FileSystem before we store the mounted jar path.
    let fs_pin = ctx.pin_native_root(fs);
    let jp = ctx.create_string(jar_path);
    let fs = ctx.read_native_pin(fs_pin, fs);
    ctx.set_field(fs, P57_FS_JAR_FIELD, Value::Object(Some(jp)));
    ctx.unpin_native_roots(fs_pin);
    Ok(fs)
}

/// Allocate a synthetic runtime-image (`jrt:`) FileSystem rooted at `java_home`.
/// `getPath`/`readAttributes`/`newDirectoryStream` route through the jrt helpers
/// when they see the P57_FS_JRT_FIELD / a `JRTFS`-encoded path.
pub fn p57_alloc_jrt_filesystem(
    ctx: &mut dyn NativeContext,
    java_home: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let fs = try_alloc_concurrent_synthetic(ctx, "java/nio/file/FileSystem", P57_FS_SLOTS)?;
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
    Ok(fs)
}

// ---------------------------------------------------------------------------
// File.deleteOnExit
// ---------------------------------------------------------------------------

/// Paths registered via `File.deleteOnExit()`, as `(vm_identity, path)` rows in
/// registration order. Each VM's rows are deleted, in reverse registration
/// order (the JDK's `DeleteOnExitHook` order), when that VM is torn down
/// ([`forget_vm_delete_on_exit`]); the `atexit` handler deletes the rows of
/// every VM still alive when the process exits.
///
/// gc-common w19-a
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// the rows carried no VM, so a host that ran VMs one after another kept every
/// disposed VM's files on disk until the host process exited, the list grew by
/// a row per registration forever, and a later VM's registration of the same
/// path was de-duplicated against a disposed VM's row.
pub static DELETE_ON_EXIT: std::sync::OnceLock<std::sync::Mutex<Vec<(usize, String)>>> =
    std::sync::OnceLock::new();

pub static DELETE_ON_EXIT_HOOK_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Delete `paths` last-registered first, so a directory registered before its
/// contents is removed after them, exactly as `DeleteOnExitHook` does. Best
/// effort, like the JDK: a failure to delete is not reported.
pub fn delete_on_exit_paths(paths: &[String]) {
    for p in paths.iter().rev() {
        if std::fs::remove_file(p).is_err() {
            let _ = std::fs::remove_dir(p);
        }
    }
}

pub extern "C" fn delete_on_exit_run() {
    let Some(lock) = DELETE_ON_EXIT.get() else {
        return;
    };
    let rows = match lock.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(e) => std::mem::take(&mut *e.into_inner()),
    };
    let paths: Vec<String> = rows.into_iter().map(|(_, path)| path).collect();
    delete_on_exit_paths(&paths);
}

/// Record `path` for deletion when VM `vm` terminates. De-duplicated per VM:
/// the JDK keeps one `LinkedHashSet` per VM.
pub fn delete_on_exit_register(vm: usize, path: String) {
    let lock = DELETE_ON_EXIT.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    {
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        if g.iter().any(|(owner, p)| *owner == vm && p == &path) {
            return;
        }
        g.push((vm, path));
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

/// VM teardown (`forget_vm_native_root_stores`): take `vm`'s `deleteOnExit`
/// rows out under the lock and delete their files after it is released, in
/// reverse registration order. Another VM's rows are untouched. Idempotent.
pub fn forget_vm_delete_on_exit(vm: usize) {
    let Some(lock) = DELETE_ON_EXIT.get() else {
        return;
    };
    let mine: Vec<String> = {
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        if !g.iter().any(|(owner, _)| *owner == vm) {
            return;
        }
        let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut *g)
            .into_iter()
            .partition(|(owner, _)| *owner == vm);
        *g = others;
        mine.into_iter().map(|(_, path)| path).collect()
    };
    delete_on_exit_paths(&mine);
}


// ---------------------------------------------------------------------------
// DosFileAttributeView setters / DosFileAttributes flag reads
// ---------------------------------------------------------------------------

/// Windows `FILE_ATTRIBUTE_*` bits used by the DOS view/attribute natives.
pub const DOS_ATTR_HIDDEN: u32 = 0x0000_0002;
pub const DOS_ATTR_SYSTEM: u32 = 0x0000_0004;
pub const DOS_ATTR_ARCHIVE: u32 = 0x0000_0020;

/// Path behind a synthetic `*FileAttributeView` (slot 0 holds the `Path`).
pub fn attr_view_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let v = ctx.get_field(this, 0);
    extract_path_string(ctx, Some(&v))
}

pub fn attr_view_flag_arg(args: &[Value]) -> bool {
    args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0
}

/// Read a NUL-terminated UTF-16 string out of native memory at `addr`.
///
/// `addr` comes from a JDK `NativeBuffer`, so it is whatever
/// `Unsafe.allocateMemory` handed out — an arena handle or a real pointer.
/// `copy_from_native_memory` is the bridge that classifies the two; going
/// straight to a raw dereference would SIGSEGV on the handle form.
///
/// Two code units at a time rather than one bulk read: the bridge fails a range
/// that is not wholly inside one live block, and the caller does not know the
/// string's length in advance. The strings here are a file extension and the
/// literal `"Content Type"`, so this is a couple of dozen reads.
#[cfg(windows)]
pub fn read_native_utf16_c_string(ctx: &dyn NativeContext, addr: i64) -> Option<String> {
    // The JDK's own ceiling for a native-buffer string; a missing terminator
    // must stop somewhere rather than walk the address space.
    const MAX_CHARS: usize = 32 * 1024;
    if addr == 0 {
        return None;
    }
    let mut units: Vec<u16> = Vec::new();
    for i in 0..MAX_CHARS {
        let mut cell = [0u8; 2];
        if !ctx.copy_from_native_memory(addr + (i as i64) * 2, &mut cell) {
            return None;
        }
        let unit = u16::from_le_bytes(cell);
        if unit == 0 {
            return Some(String::from_utf16_lossy(&units));
        }
        units.push(unit);
    }
    None
}

/// `HKEY_CLASSES_ROOT\<sub_key>` → the named `REG_SZ` value, or `None`.
///
/// This is what the JDK's `RegistryFileTypeDetector` JNI does, and it has to be
/// the same registry: the answers differ from the `content-types.properties`
/// fallback in ways the HotSpot comparison shows — `.zip` is
/// `application/x-zip-compressed` here and `application/zip` there, `.xml` is
/// `text/xml` here and `application/xml` there.
#[cfg(windows)]
pub fn hkcr_string_value(sub_key: &str, value_name: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;

    // `((HKEY)(ULONG_PTR)((LONG)0x80000000))` — sign-extended on 64-bit, which
    // is why this goes through `i32` rather than being written as a `usize`.
    const HKEY_CLASSES_ROOT: isize = 0x8000_0000u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const ERROR_SUCCESS: i32 = 0;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: isize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            value_type: *mut u32,
            data: *mut u16,
            data_bytes: *mut u32,
        ) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    // A NUL inside either name would truncate the key silently; the registry
    // has no such names, so refuse rather than query a different key.
    if sub_key.contains('\0') || value_name.contains('\0') {
        return None;
    }
    let key = wide(sub_key);
    let name = wide(value_name);

    // Size query first — the value is arbitrary user-writable registry data,
    // so a fixed buffer would be a guess about someone else's content.
    let mut bytes: u32 = 0;
    // SAFETY: both name buffers are NUL-terminated and outlive the call, and
    // this form asks only for the size (null data pointer), which is what the
    // API documents for `pvData == NULL`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if rc != ERROR_SUCCESS || bytes == 0 {
        return None;
    }
    // `bytes` counts BYTES and includes the terminator. Round up so an odd
    // (malformed) length still gets a whole last code unit to land in.
    let mut buf: Vec<u16> = vec![0; (bytes as usize).div_ceil(2)];
    let mut got = bytes;
    // SAFETY: `buf` is sized from the length the same API just reported, and
    // `got` tells it that size in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            &mut got,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let units = ((got as usize) / 2).min(buf.len());
    let end = buf[..units].iter().position(|&u| u == 0).unwrap_or(units);
    let value = String::from_utf16_lossy(&buf[..end]);
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// `GetFileAttributesExW(path, GetFileExInfoStandard, &data)`, returning the
/// raw 36-byte `WIN32_FILE_ATTRIBUTE_DATA` struct on success or the Win32
/// error code (`GetLastError()`) on failure.
///
/// The struct layout is not this VM's choice: it is exactly what real
/// `sun.nio.fs.WindowsFileAttributes.fromFileAttributeData(long, int)`
/// decodes via `Unsafe.getInt`/`getLong` at fixed offsets (`javap -c`:
/// offset 0 `int dwFileAttributes`, 4 `long ftCreationTime`, 12
/// `long ftLastAccessTime`, 20 `long ftLastWriteTime`, 28
/// `int nFileSizeHigh`, 32 `int nFileSizeLow` — 36 bytes, matching
/// `WindowsFileAttributes.get`'s own `NativeBuffers.getNativeBuffer(36)`
/// exactly). A `FILETIME`'s two `DWORD`s (`dwLowDateTime` at the lower
/// address, `dwHighDateTime` above it) read back as one little-endian `i64`
/// on this architecture, which is why Java reads it with a single
/// `getLong` rather than two `getInt`s — this function returns the raw
/// bytes in exactly that order so no repacking is needed on the Java side.
#[cfg(windows)]
pub fn win32_get_file_attributes_ex(path: &[u16]) -> Result<[u8; 36], u32> {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct Win32FileAttributeData {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        file_size_high: u32,
        file_size_low: u32,
    }
    const GET_FILE_EX_INFO_STANDARD: i32 = 0;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileAttributesExW(
            file_name: *const u16,
            info_level_id: i32,
            file_information: *mut c_void,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    let mut data = Win32FileAttributeData {
        file_attributes: 0,
        creation_time: FileTime { low: 0, high: 0 },
        last_access_time: FileTime { low: 0, high: 0 },
        last_write_time: FileTime { low: 0, high: 0 },
        file_size_high: 0,
        file_size_low: 0,
    };
    // SAFETY: `path` is a NUL-terminated UTF-16 buffer the caller owns for
    // the duration of this call (read once, not retained), and `data` is a
    // stack-local struct of exactly the size and layout the API's
    // `GetFileExInfoStandard` level writes — verified above against the
    // real JDK's own decode offsets, not assumed.
    let ok = unsafe {
        GetFileAttributesExW(
            path.as_ptr(),
            GET_FILE_EX_INFO_STANDARD,
            &mut data as *mut _ as *mut c_void,
        )
    };
    if ok == 0 {
        return Err(unsafe { GetLastError() });
    }
    let mut out = [0u8; 36];
    out[0..4].copy_from_slice(&data.file_attributes.to_le_bytes());
    out[4..8].copy_from_slice(&data.creation_time.low.to_le_bytes());
    out[8..12].copy_from_slice(&data.creation_time.high.to_le_bytes());
    out[12..16].copy_from_slice(&data.last_access_time.low.to_le_bytes());
    out[16..20].copy_from_slice(&data.last_access_time.high.to_le_bytes());
    out[20..24].copy_from_slice(&data.last_write_time.low.to_le_bytes());
    out[24..28].copy_from_slice(&data.last_write_time.high.to_le_bytes());
    out[28..32].copy_from_slice(&data.file_size_high.to_le_bytes());
    out[32..36].copy_from_slice(&data.file_size_low.to_le_bytes());
    Ok(out)
}

/// `CreateFileW(path, dwDesiredAccess, dwShareMode, lpSecurityAttributes,
/// dwCreationDisposition, dwFlagsAndAttributes, NULL)`, returning the
/// `HANDLE` as a raw `i64` on success or the Win32 error code
/// (`GetLastError()`) on failure.
///
/// Parameter order and meaning are exactly `WindowsNativeDispatcher`'s own
/// `private static native long CreateFile0(long, int, int, long, int, int)`
/// (`javap -c` on `WindowsNativeDispatcher.CreateFile(String,int,int,long,
/// int,int)`: path buffer address, dwDesiredAccess, dwShareMode,
/// lpSecurityAttributes, dwCreationDisposition, dwFlagsAndAttributes, in that
/// order). `hTemplateFile` is never exposed at the Java level (every real
/// caller passes `NULL`), so it is fixed here too. Every observed real-JDK
/// call site passes `lpSecurityAttributes == 0` (`WindowsChannelFactory`'s
/// four-arg `CreateFile` overload hardcodes `lconst_0`); a non-zero value is
/// refused rather than silently mistreated, since it could be a tagged arena
/// handle from this VM's synthetic memory model and treating it as a raw
/// `SECURITY_ATTRIBUTES*` would be a memory-safety bug, not just a wrong
/// answer.
#[cfg(windows)]
pub fn win32_create_file(
    path: &[u16],
    desired_access: u32,
    share_mode: u32,
    security_attributes: i64,
    creation_disposition: u32,
    flags_and_attributes: u32,
) -> Result<i64, WindowsCreateFileError> {
    use std::ffi::c_void;

    if security_attributes != 0 {
        return Err(WindowsCreateFileError::UnsupportedSecurityAttributes);
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> i64;
        fn GetLastError() -> u32;
    }

    const INVALID_HANDLE_VALUE: i64 = -1;

    // SAFETY: `path` is a NUL-terminated UTF-16 buffer the caller owns for
    // the duration of this call; every pointer argument below is either
    // that buffer or a hardcoded `NULL`, matching the real API's contract
    // for a security-attributes-free, template-free open.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            desired_access,
            share_mode,
            std::ptr::null_mut(),
            creation_disposition,
            flags_and_attributes,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(WindowsCreateFileError::Win32(unsafe { GetLastError() }));
    }
    Ok(handle)
}

#[cfg(windows)]
pub enum WindowsCreateFileError {
    Win32(u32),
    UnsupportedSecurityAttributes,
}

/// `GetFullPathNameW(path, ...)`, returning the resolved path verbatim (no
/// trailing-separator trimming, no `/`-to-`\` massaging beyond what the Win32
/// API itself performs) on success or the Win32 error code (`GetLastError()`)
/// on failure.
///
/// Deliberately NOT `win_get_full_path_name` above: that helper post-processes
/// its result to match real HotSpot `WinNTFileSystem.canonicalize`'s output
/// shape, but `WindowsNativeDispatcher.GetFullPathName0` has no such
/// post-processing in real bytecode (`javap -c` on the wrapper: call, then a
/// bare `areturn` in the non-exceptional path) — real `WindowsPathParser`/
/// `WindowsLinkSupport` callers expect the raw API answer, trailing separator
/// and all.
#[cfg(windows)]
pub fn win32_get_full_path_name0(path: &[u16]) -> Result<String, u32> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFullPathNameW(
            lp_file_name: *const u16,
            n_buffer_length: u32,
            lp_buffer: *mut u16,
            lp_file_part: *mut *mut u16,
        ) -> u32;
        fn GetLastError() -> u32;
    }
    // SAFETY: `path` is a NUL-terminated UTF-16 buffer the caller owns for
    // the duration of this call. The first call passes a null buffer to
    // learn the required length (including NUL); the second call writes
    // into a stack-owned `Vec` sized from that answer.
    unsafe {
        let needed = GetFullPathNameW(path.as_ptr(), 0, std::ptr::null_mut(), std::ptr::null_mut());
        if needed == 0 {
            return Err(GetLastError());
        }
        let mut buf: Vec<u16> = vec![0u16; needed as usize];
        let written = GetFullPathNameW(
            path.as_ptr(),
            buf.len() as u32,
            buf.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        if written == 0 || written as usize >= buf.len() {
            return Err(GetLastError());
        }
        buf.truncate(written as usize);
        Ok(String::from_utf16_lossy(&buf))
    }
}

/// `GetFileAttributesW(path)`, returning the raw `DWORD` attributes bitmask
/// on success (`INVALID_FILE_ATTRIBUTES`, i.e. `0xFFFFFFFF`, is Win32's own
/// failure sentinel and is turned into the `GetLastError()` code instead of
/// being returned as data) or the Win32 error code on failure.
#[cfg(windows)]
pub fn win32_get_file_attributes0(path: &[u16]) -> Result<u32, u32> {
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileAttributesW(lp_file_name: *const u16) -> u32;
        fn GetLastError() -> u32;
    }
    // SAFETY: `path` is a NUL-terminated UTF-16 buffer the caller owns for
    // the duration of this call.
    let attrs = unsafe { GetFileAttributesW(path.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(unsafe { GetLastError() });
    }
    Ok(attrs)
}

/// `FindFirstFileW(path, &data)`, returning `(handle, dwFileAttributes,
/// cFileName)` on success or the Win32 error code on failure. The caller
/// owns the returned search handle and must eventually pass it to
/// `win32_find_close` (mirroring real `WindowsNativeDispatcher.FindClose`) —
/// this function does not close it itself, matching the real JDK's own
/// split between `FindFirstFile0` (opens) and `FindClose` (closes), which
/// `WindowsLinkSupport.getRealPath`'s per-component walk calls across two
/// separate native invocations.
#[cfg(windows)]
pub fn win32_find_first_file(path: &[u16]) -> Result<(i64, u32, String, Vec<u8>), u32> {
    use std::ffi::c_void;

    // Same `WIN32_FIND_DATAW` shape and the same `FindFirstFileW`/
    // `FindClose` extern signatures `win_case_correct` below declares for
    // its own, unrelated call site — kept as separate local declarations
    // (matching signatures exactly, or `-D clashing-extern-declarations`
    // refuses the build) rather than shared, since the two call sites serve
    // different callers (this one backs a real JNI-declared native; that
    // one is a pure-Rust canonicalize helper) and have no reason to change
    // together.
    #[repr(C)]
    struct FileTime {
        _low: u32,
        _high: u32,
    }
    #[repr(C)]
    struct Win32FindDataW {
        file_attributes: u32,
        _creation_time: FileTime,
        _last_access_time: FileTime,
        _last_write_time: FileTime,
        _file_size_high: u32,
        _file_size_low: u32,
        _reserved0: u32,
        _reserved1: u32,
        file_name: [u16; 260],
        _alternate_file_name: [u16; 14],
    }
    extern "system" {
        fn FindFirstFileW(
            file_name: *const u16,
            find_file_data: *mut Win32FindDataW,
        ) -> *mut c_void;
        fn GetLastError() -> u32;
    }
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    // SAFETY: `path` is a NUL-terminated UTF-16 buffer the caller owns for
    // the duration of this call, and `data` is a stack-local struct of
    // exactly the size and layout `WIN32_FIND_DATAW` requires (no packing
    // pragma on the real struct, so `#[repr(C)]`'s natural alignment
    // matches it field-for-field).
    let mut data: Win32FindDataW = unsafe { std::mem::zeroed() };
    let handle = unsafe { FindFirstFileW(path.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(unsafe { GetLastError() });
    }
    let name_len = data
        .file_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.file_name.len());
    let name = String::from_utf16_lossy(&data.file_name[..name_len]);
    // SAFETY: `data` is a live, fully initialised `#[repr(C)]` struct of
    // `size_of` bytes with no padding-sensitive reads on the Java side.
    let raw = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!(data).cast::<u8>(),
            std::mem::size_of::<Win32FindDataW>(),
        )
    }
    .to_vec();
    Ok((handle as i64, data.file_attributes, name, raw))
}

/// `FindNextFileW` on a search handle [`win32_find_first_file`] opened:
/// `Ok(Some((name, raw WIN32_FIND_DATAW)))` for the next entry, `Ok(None)` once
/// `ERROR_NO_MORE_FILES` is reached, `Err(code)` for any other failure — the
/// three outcomes the JNI `FindNextFile0` maps to a String, NULL and a throw.
#[cfg(windows)]
pub fn win32_find_next_file(handle: i64) -> Result<Option<(String, Vec<u8>)>, u32> {
    use std::ffi::c_void;
    #[repr(C)]
    struct FileTime {
        _low: u32,
        _high: u32,
    }
    #[repr(C)]
    struct Win32FindDataW {
        _file_attributes: u32,
        _creation_time: FileTime,
        _last_access_time: FileTime,
        _last_write_time: FileTime,
        _file_size_high: u32,
        _file_size_low: u32,
        _reserved0: u32,
        _reserved1: u32,
        file_name: [u16; 260],
        _alternate_file_name: [u16; 14],
    }
    extern "system" {
        fn FindNextFileW(h_find_file: *mut c_void, find_file_data: *mut Win32FindDataW) -> i32;
        fn GetLastError() -> u32;
    }
    const ERROR_NO_MORE_FILES: u32 = 18;
    // SAFETY: `handle` is a search handle this VM opened and has not closed;
    // `data` is a stack-local `WIN32_FIND_DATAW`-shaped struct.
    let mut data: Win32FindDataW = unsafe { std::mem::zeroed() };
    let ok = unsafe { FindNextFileW(handle as *mut c_void, &mut data) };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        return if err == ERROR_NO_MORE_FILES {
            Ok(None)
        } else {
            Err(err)
        };
    }
    let name_len = data
        .file_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.file_name.len());
    let name = String::from_utf16_lossy(&data.file_name[..name_len]);
    // SAFETY: as in `win32_find_first_file`.
    let raw = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!(data).cast::<u8>(),
            std::mem::size_of::<Win32FindDataW>(),
        )
    }
    .to_vec();
    Ok(Some((name, raw)))
}

/// `FindClose(handle)` on a search handle `win32_find_first_file` opened.
#[cfg(windows)]
pub fn win32_find_close(handle: i64) -> Result<(), u32> {
    use std::ffi::c_void;
    extern "system" {
        fn FindClose(h_find_file: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }
    // SAFETY: `handle` is a search handle this VM received from a prior
    // `win32_find_first_file` call and has not closed since.
    let ok = unsafe { FindClose(handle as *mut c_void) };
    if ok == 0 {
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

/// Build the `sun.nio.fs.WindowsException(int)` a failed Win32 call throws —
/// the same construction every native above does inline.
#[cfg(windows)]
pub fn win32_exception(ctx: &mut dyn NativeContext, code: u32, what: &str) -> MethodCallFailed {
    match ctx.new_object_initialized(
        "sun/nio/fs/WindowsException",
        "(I)V",
        &[Value::Int(code as i32)],
    ) {
        Ok(Some(Value::Object(Some(exc_ref)))) => MethodCallFailed::ExceptionThrown(exc_ref),
        _ => RuntimeError::IOException {
            message: format!("{what} failed, Win32 error {code}"),
        }
        .into(),
    }
}

/// The body shared by every path-only `WindowsNativeDispatcher` native: the
/// leading `npaths` parameters are `NativeBuffer` addresses of NUL-terminated
/// UTF-16 paths (the convention `CreateFile0`/`GetFileAttributesEx0` already
/// use) and the result is `void`-or-throw. `op` receives the decoded paths
/// (each with its NUL) and the raw argument list, and returns the Win32 error
/// code on failure.
#[cfg(windows)]
pub fn win32_path_native_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    npaths: usize,
    op: fn(&[Vec<u16>], &[Value]) -> Result<(), u32>,
) -> MethodCallResult {
    let mut paths: Vec<Vec<u16>> = Vec::with_capacity(npaths);
    for i in 0..npaths {
        let addr = match args.get(i) {
            Some(Value::Long(v)) => *v,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some(format!("{name}: path buffer address {i} is not a long")),
                }
                .into())
            }
        };
        let text = match read_native_utf16_c_string(ctx, addr) {
            Some(t) => t,
            None => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("{name}: unreadable path buffer {i}"),
                }
                .into())
            }
        };
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        paths.push(wide);
    }
    match op(&paths, args) {
        Ok(()) => Ok(None),
        Err(code) => Err(win32_exception(ctx, code, name)),
    }
}

/// Turn a Win32 `BOOL`/`BOOLEAN` result into the `Result` shape `op`s return.
#[cfg(windows)]
pub fn win32_check(ok: bool) -> Result<(), u32> {
    extern "system" {
        fn GetLastError() -> u32;
    }
    if ok {
        Ok(())
    } else {
        // SAFETY: reads the calling thread's last-error slot; no arguments.
        Err(unsafe { GetLastError() })
    }
}

/// The path-only `WindowsNativeDispatcher` natives the link/copy/move/delete
/// surface of `WindowsFileSystemProvider` reaches (lane-4 wave 15). Each is a
/// one-to-one kernel32 call over the decoded paths, with the JDK's own argument
/// order (`javap -s` on the class; the C bodies in `WindowsNativeDispatcher.c`).
#[cfg(windows)]
pub fn win32_register_link_natives(r: &mut NativeMethodRegistry) {
    use std::ffi::c_void;
    extern "system" {
        fn CreateSymbolicLinkW(lp_symlink: *const u16, lp_target: *const u16, flags: u32) -> u8;
        fn CreateHardLinkW(
            lp_file_name: *const u16,
            lp_existing: *const u16,
            lp_security_attributes: *mut c_void,
        ) -> i32;
        fn DeleteFileW(lp_file_name: *const u16) -> i32;
        fn RemoveDirectoryW(lp_path_name: *const u16) -> i32;
        fn CreateDirectoryW(lp_path_name: *const u16, lp_security_attributes: *mut c_void) -> i32;
        fn MoveFileExW(lp_existing: *const u16, lp_new: *const u16, flags: u32) -> i32;
        fn CopyFileExW(
            lp_existing: *const u16,
            lp_new: *const u16,
            routine: *mut c_void,
            data: *mut c_void,
            cancel: *mut i32,
            flags: u32,
        ) -> i32;
        fn SetFileAttributesW(lp_file_name: *const u16, dw_file_attributes: u32) -> i32;
    }
    fn int_arg(args: &[Value], i: usize) -> u32 {
        match args.get(i) {
            Some(Value::Int(v)) => *v as u32,
            _ => 0,
        }
    }
    // `private static native void CreateSymbolicLink0(long, long, int)`:
    // (link, target, flags). Fails with `ERROR_PRIVILEGE_NOT_HELD` (1314) on a
    // host that does not grant the privilege — which `Files.createSymbolicLink`
    // reports as a `FileSystemException`, exactly as HotSpot does.
    win32_register_path_native!(r, "CreateSymbolicLink0", "(JJI)V", 2, |p, a| {
        // SAFETY: both buffers are NUL-terminated UTF-16 built by
        // `win32_register_path_native`.
        win32_check(
            unsafe { CreateSymbolicLinkW(p[0].as_ptr(), p[1].as_ptr(), int_arg(a, 2)) } != 0,
        )
    });
    // `CreateHardLink0(long newFile, long existingFile)`.
    win32_register_path_native!(r, "CreateHardLink0", "(JJ)V", 2, |p, _| {
        // SAFETY: as above; a null security descriptor is what the JDK passes.
        win32_check(
            unsafe { CreateHardLinkW(p[0].as_ptr(), p[1].as_ptr(), std::ptr::null_mut()) } != 0,
        )
    });
    win32_register_path_native!(r, "DeleteFile0", "(J)V", 1, |p, _| {
        // SAFETY: as above.
        win32_check(unsafe { DeleteFileW(p[0].as_ptr()) } != 0)
    });
    win32_register_path_native!(r, "RemoveDirectory0", "(J)V", 1, |p, _| {
        // SAFETY: as above.
        win32_check(unsafe { RemoveDirectoryW(p[0].as_ptr()) } != 0)
    });
    // `CreateDirectory0(long path, long securityDescriptor)`: the JDK builds a
    // SECURITY_ATTRIBUTES from a non-zero descriptor; this VM cannot, so a
    // non-zero one is refused the way `CreateFile0` refuses it rather than
    // silently created with default permissions.
    win32_register_path_native!(r, "CreateDirectory0", "(JJ)V", 1, |p, a| {
        if matches!(a.get(1), Some(Value::Long(v)) if *v != 0) {
            return Err(87); // ERROR_INVALID_PARAMETER
        }
        // SAFETY: as above.
        win32_check(unsafe { CreateDirectoryW(p[0].as_ptr(), std::ptr::null_mut()) } != 0)
    });
    // `MoveFileEx0(long existing, long new, int flags)`.
    win32_register_path_native!(r, "MoveFileEx0", "(JJI)V", 2, |p, a| {
        // SAFETY: as above.
        win32_check(unsafe { MoveFileExW(p[0].as_ptr(), p[1].as_ptr(), int_arg(a, 2)) } != 0)
    });
    // `CopyFileEx0(long existing, long new, int flags, long cancelFlagAddr)`:
    // the cancel flag is a JDK-side `int*` polled by another thread; nothing in
    // this VM sets it, so it is passed as NULL (never cancelled).
    win32_register_path_native!(r, "CopyFileEx0", "(JJIJ)V", 2, |p, a| {
        // SAFETY: as above; null routine/data/cancel pointers are legal.
        win32_check(
            unsafe {
                CopyFileExW(
                    p[0].as_ptr(),
                    p[1].as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    int_arg(a, 2),
                )            } != 0,
        )
    });
    win32_register_path_native!(r, "SetFileAttributes0", "(JI)V", 1, |p, a| {
        // SAFETY: as above.
        win32_check(unsafe { SetFileAttributesW(p[0].as_ptr(), int_arg(a, 1)) } != 0)
    });
    // Handle-taking natives. A `HANDLE` Java holds is an fd_table id, not a raw
    // OS handle (`CreateFile0` absorbs the real one — see
    // `FdTable::insert_win32_file`), so each of these resolves the id to a
    // duplicate of the underlying `File` first.
    //
    // `private static native void GetFileInformationByHandle0(long handle,
    // long address)`: fills the 52-byte `BY_HANDLE_FILE_INFORMATION`
    // `WindowsFileAttributes.fromByHandleFileInformation` decodes by fixed offset.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetFileInformationByHandle0",
        "(JJ)V",
        |ctx, args| {
            extern "system" {
                fn GetFileInformationByHandle(h: *mut c_void, info: *mut u8) -> i32;
            }
            let (handle, addr) = match (args.first(), args.get(1)) {
                (Some(Value::Long(h)), Some(Value::Long(a))) => (*h, *a),
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "GetFileInformationByHandle0: bad arguments".into(),
                    }
                    .into())
                }
            };
            let file = match ctx.fd_table().clone_file(handle as _) {
                Ok(f) => f,
                Err(_) => return Err(win32_exception(ctx, 6, "GetFileInformationByHandle0")),
            };
            use std::os::windows::io::AsRawHandle;
            let mut info = [0u8; 52];
            // SAFETY: `info` is the 52-byte BY_HANDLE_FILE_INFORMATION and
            // `file` owns a live duplicate of the handle for the whole call.
            let ok = unsafe {
                GetFileInformationByHandle(file.as_raw_handle().cast(), info.as_mut_ptr())
            };
            if let Err(code) = win32_check(ok != 0) {
                return Err(win32_exception(ctx, code, "GetFileInformationByHandle0"));
            }
            if !ctx.copy_to_native_memory(addr, &info) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "GetFileInformationByHandle0: unwritable result buffer".into(),
                }
                .into());
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native long GetFileSizeEx(long handle)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetFileSizeEx",
        "(J)J",
        |ctx, args| {
            let handle = match args.first() {
                Some(Value::Long(h)) => *h,
                _ => 0,
            };
            match ctx.fd_table().file_size(handle as _) {
                Ok(n) => Ok(Some(Value::Long(n as i64))),
                Err(_) => Err(win32_exception(ctx, 6, "GetFileSizeEx")),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void CloseHandle(long handle)`: releases the fd_table entry
    // the handle stands for. Never throws (the JNI body ignores the result).
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "CloseHandle",
        "(J)V",
        |ctx, args| {
            if let Some(Value::Long(h)) = args.first() {
                // A token handle `OpenProcessToken`/`DuplicateTokenEx` minted
                // (see `Win32SecurityHandles`) is not an fd_table id: release
                // the OS handle instead of closing an unrelated descriptor.
                if !win32_close_token_handle(*h) {
                    let _ = ctx.fd_table().close(*h as _);                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native String FormatMessage(int errorCode)`: reached by EVERY
    // failing call above, because `WindowsException.errorString` asks for the
    // system's text before `translateToIOException` picks the exception type.
    // The JNI body formats into a 255-unit buffer with only
    // `FORMAT_MESSAGE_FROM_SYSTEM`, returns NULL when the code has no text, and
    // drops a trailing `\n`, `\r` and `.` when the message is longer than 3.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "FormatMessage",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            extern "system" {
                fn FormatMessageW(
                    flags: u32,
                    source: *const c_void,
                    message_id: u32,
                    language_id: u32,
                    buffer: *mut u16,
                    size: u32,
                    arguments: *mut c_void,
                ) -> u32;
            }
            const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x0000_1000;
            let code = match args.first() {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let mut buf = [0u16; 255];
            // SAFETY: `buf` is a live 255-unit buffer and its length is passed;
            // no source, no arguments.
            let mut len = unsafe {
                FormatMessageW(
                    FORMAT_MESSAGE_FROM_SYSTEM,
                    std::ptr::null(),
                    code,
                    0,
                    buf.as_mut_ptr(),
                    255,
                    std::ptr::null_mut(),
                )
            } as usize;
            if len == 0 {
                return Ok(Some(Value::Object(None)));
            }
            if len > 3 {
                if buf[len - 1] == u16::from(b'\n') {
                    len -= 1;
                }
                if buf[len - 1] == u16::from(b'\r') {
                    len -= 1;
                }
                if buf[len - 1] == u16::from(b'.') {
                    len -= 1;
                }
            }
            let text = String::from_utf16_lossy(&buf[..len]);
            let s = ctx.create_string(&text);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

/// Read the `i`-th argument as a `long`, or throw the NPE every native here uses.
#[cfg(windows)]
pub fn win32_long_arg(args: &[Value], i: usize, what: &str) -> Result<i64, MethodCallFailed> {
    match args.get(i) {
        Some(Value::Long(v)) => Ok(*v),
        _ => Err(RuntimeError::NullPointerException {
            message: Some(format!("{what}: argument {i} is not a long")),
        }
        .into()),
    }
}

/// Decode a NUL-terminated UTF-16 `NativeBuffer` argument into a wide string
/// WITH its terminator, ready for a `*W` call.
#[cfg(windows)]
pub fn win32_wide_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
    i: usize,
    what: &str,
) -> Result<Vec<u16>, MethodCallFailed> {
    let addr = win32_long_arg(args, i, what)?;
    let text = read_native_utf16_c_string(ctx, addr).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::IllegalArgumentException {
            message: format!("{what}: unreadable path buffer {i}"),
        })
    })?;
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    Ok(wide)
}

/// Resolve a Java-held `HANDLE` (an fd_table id — see `CreateFile0`) to a
/// duplicate of the file it stands for. The duplicate is closed when it drops;
/// the fd_table entry stays open for `CloseHandle` to release.
#[cfg(windows)]
pub fn win32_handle_file(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    what: &str,
) -> Result<std::fs::File, MethodCallFailed> {
    let h = win32_long_arg(args, 0, what)?;
    match ctx.fd_table().clone_file(h as _) {
        Ok(f) => Ok(f),
        Err(_) => Err(win32_exception(ctx, 6, what)), // ERROR_INVALID_HANDLE
    }
}

/// The filesystem-shaped `WindowsNativeDispatcher` natives beyond the path-only
/// ones (lane-4 wave 16): directory search continuation, volume and drive
/// queries, and the handle-taking ones (`SetFileTime0`, `SetEndOfFile`,
/// `DeviceIoControl*`, `GetFinalPathNameByHandle`). Each is the JNI body of
/// `WindowsNativeDispatcher.c` reproduced over kernel32.
#[cfg(windows)]
pub fn win32_register_fs_natives(r: &mut NativeMethodRegistry) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    extern "system" {
        fn GetVolumePathNameW(file_name: *const u16, buffer: *mut u16, len: u32) -> i32;
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
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
        fn GetDiskFreeSpaceW(
            lpRootPathName: *const u16,
            lpSectorsPerCluster: *mut u32,
            lpBytesPerSector: *mut u32,
            lpNumberOfFreeClusters: *mut u32,
            lpTotalNumberOfClusters: *mut u32,
        ) -> i32;
        fn GetLogicalDrives() -> u32;
        fn SetFileTime(
            file: *mut c_void,
            creation: *const FileTime,
            access: *const FileTime,
            modified: *const FileTime,
        ) -> i32;
        fn SetEndOfFile(file: *mut c_void) -> i32;
        fn DeviceIoControl(
            device: *mut c_void,
            control_code: u32,
            in_buffer: *mut c_void,
            in_size: u32,
            out_buffer: *mut c_void,
            out_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn GetFinalPathNameByHandleW(
            file: *mut c_void,
            buffer: *mut u16,
            len: u32,
            flags: u32,
        ) -> u32;
        fn GetLastError() -> u32;
    }
    fn utf16_until_nul(buf: &[u16]) -> String {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }

    // `private static native long FindFirstFile1(long path, long data)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "FindFirstFile1",
        "(JJ)J",
        |ctx, args| {
            let wide = win32_wide_arg(ctx, args, 0, "FindFirstFile1")?;
            let data_addr = win32_long_arg(args, 1, "FindFirstFile1")?;
            match win32_find_first_file(&wide) {
                Ok((handle, _attrs, _name, raw)) => {
                    let _ = ctx.copy_to_native_memory(data_addr, &raw);
                    Ok(Some(Value::Long(handle)))
                }
                Err(code) => Err(win32_exception(ctx, code, "FindFirstFile1")),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native String FindNextFile0(long handle, long data)`:
    // the next entry's name, NULL at the end of the listing, a throw otherwise.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "FindNextFile0",
        "(JJ)Ljava/lang/String;",
        |ctx, args| {
            let handle = win32_long_arg(args, 0, "FindNextFile0")?;
            let data_addr = win32_long_arg(args, 1, "FindNextFile0")?;
            match win32_find_next_file(handle) {
                Ok(Some((name, raw))) => {
                    let _ = ctx.copy_to_native_memory(data_addr, &raw);
                    let s = ctx.create_string(&name);
                    Ok(Some(Value::Object(Some(s))))
                }
                Ok(None) => Ok(Some(Value::Object(None))),
                Err(code) => Err(win32_exception(ctx, code, "FindNextFile0")),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native String GetVolumePathName0(long path)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetVolumePathName0",
        "(J)Ljava/lang/String;",
        |ctx, args| {
            let wide = win32_wide_arg(ctx, args, 0, "GetVolumePathName0")?;
            let mut buf = [0u16; 1024];
            // SAFETY: `wide` is NUL-terminated; `buf` is 1024 units and that
            // length is passed.
            let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), buf.as_mut_ptr(), 1024) };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "GetVolumePathName0"));
            }
            let s = ctx.create_string(&utf16_until_nul(&buf));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native void GetVolumeInformation0(long root,
    // VolumeInformation out)`: fills `fileSystemName`, `volumeName`,
    // `volumeSerialNumber`, `flags`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetVolumeInformation0",
        "(JLsun/nio/fs/WindowsNativeDispatcher$VolumeInformation;)V",
        |ctx, args| {
            let wide = win32_wide_arg(ctx, args, 0, "GetVolumeInformation0")?;
            let out = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("GetVolumeInformation0: null out-parameter".into()),
                    }
                    .into())
                }
            };
            let mut vol = [0u16; 261];
            let mut fsn = [0u16; 261];
            let (mut serial, mut max_comp, mut flags) = (0u32, 0u32, 0u32);
            // SAFETY: both name buffers are 261 units, lengths passed.
            let ok = unsafe {
                GetVolumeInformationW(
                    wide.as_ptr(),
                    vol.as_mut_ptr(),
                    261,
                    &mut serial,
                    &mut max_comp,
                    &mut flags,
                    fsn.as_mut_ptr(),
                    261,
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "GetVolumeInformation0"));
            }
            let pin = ctx.pin_native_root(out);
            let fs_name = ctx.create_string(&utf16_until_nul(&fsn));
            let fs_pin = ctx.pin_native_root(fs_name);
            let vol_name = ctx.create_string(&utf16_until_nul(&vol));
            let fs_name = ctx.read_native_pin(fs_pin, fs_name);
            let out = ctx.read_native_pin(pin, out);
            ctx.set_field_by_name(out, "fileSystemName", Value::Object(Some(fs_name)));
            ctx.set_field_by_name(out, "volumeName", Value::Object(Some(vol_name)));
            ctx.set_field_by_name(out, "volumeSerialNumber", Value::Int(serial as i32));
            ctx.set_field_by_name(out, "flags", Value::Int(flags as i32));
            ctx.unpin_native_roots(pin);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native int GetDriveType0(long root)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetDriveType0",
        "(J)I",
        |ctx, args| {
            let wide = win32_wide_arg(ctx, args, 0, "GetDriveType0")?;
            // SAFETY: NUL-terminated wide string.
            Ok(Some(Value::Int(
                unsafe { GetDriveTypeW(wide.as_ptr()) } as i32
            )))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native void GetDiskFreeSpaceEx0(long path, DiskFreeSpace out)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetDiskFreeSpaceEx0",
        "(JLsun/nio/fs/WindowsNativeDispatcher$DiskFreeSpace;)V",
        |ctx, args| {
            let wide = win32_wide_arg(ctx, args, 0, "GetDiskFreeSpaceEx0")?;
            let out = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("GetDiskFreeSpaceEx0: null out-parameter".into()),
                    }
                    .into())
                }
            };
            let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
            // SAFETY: three live u64 out-params.
            let ok =
                unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "GetDiskFreeSpaceEx0"));
            }
            ctx.set_field_by_name(out, "freeBytesAvailable", Value::Long(avail as i64));
            ctx.set_field_by_name(out, "totalNumberOfBytes", Value::Long(total as i64));
            ctx.set_field_by_name(out, "totalNumberOfFreeBytes", Value::Long(free as i64));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native void GetDiskFreeSpace0(long root, DiskFreeSpace out)`:
    // the pre-`Ex` API; the JNI body derives the byte counts from clusters.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetDiskFreeSpace0",
        "(JLsun/nio/fs/WindowsNativeDispatcher$DiskFreeSpace;)V",
        |ctx, args| {
            let wide = win32_wide_arg(ctx, args, 0, "GetDiskFreeSpace0")?;
            let out = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("GetDiskFreeSpace0: null out-parameter".into()),
                    }
                    .into())
                }
            };
            let (mut spc, mut bps, mut free_c, mut total_c) = (0u32, 0u32, 0u32, 0u32);
            // SAFETY: four live u32 out-params.
            let ok = unsafe {
                GetDiskFreeSpaceW(wide.as_ptr(), &mut spc, &mut bps, &mut free_c, &mut total_c)
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "GetDiskFreeSpace0"));
            }
            let cluster = u64::from(spc) * u64::from(bps);
            let free_bytes = (u64::from(free_c) * cluster) as i64;
            ctx.set_field_by_name(out, "freeBytesAvailable", Value::Long(free_bytes));
            ctx.set_field_by_name(
                out,
                "totalNumberOfBytes",
                Value::Long((u64::from(total_c) * cluster) as i64),
            );
            ctx.set_field_by_name(out, "totalNumberOfFreeBytes", Value::Long(free_bytes));
            ctx.set_field_by_name(out, "bytesPerSector", Value::Long(i64::from(bps)));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native int GetLogicalDrives()`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetLogicalDrives",
        "()I",
        |ctx, _args| {
            // SAFETY: no arguments.
            let mask = unsafe { GetLogicalDrives() };
            if mask == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "GetLogicalDrives"));
            }
            Ok(Some(Value::Int(mask as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native void SetFileTime0(long handle, long create,
    // long access, long write)`: a FILETIME as a raw 64-bit tick count, `-1`
    // meaning "leave unchanged" (the JNI body passes NULL for it).
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "SetFileTime0",
        "(JJJJ)V",
        |ctx, args| {
            let file = win32_handle_file(ctx, args, "SetFileTime0")?;
            let mut times: [Option<FileTime>; 3] = [None, None, None];
            for (slot, idx) in times.iter_mut().zip(1usize..) {
                let ticks = win32_long_arg(args, idx, "SetFileTime0")?;
                if ticks != -1 {
                    *slot = Some(FileTime {
                        low: ticks as u32,
                        high: ((ticks as u64) >> 32) as u32,
                    });
                }
            }
            let ptr = |t: &Option<FileTime>| t.as_ref().map_or(std::ptr::null(), |f| f as *const _);
            // SAFETY: `file` owns a live duplicate handle; each pointer is null
            // or points into `times`, which outlives the call.
            let ok = unsafe {
                SetFileTime(
                    file.as_raw_handle().cast(),
                    ptr(&times[0]),
                    ptr(&times[1]),
                    ptr(&times[2]),
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "SetFileTime0"));            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void SetEndOfFile(long handle)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "SetEndOfFile",
        "(J)V",
        |ctx, args| {
            let file = win32_handle_file(ctx, args, "SetEndOfFile")?;
            // SAFETY: live duplicate handle.
            if unsafe { SetEndOfFile(file.as_raw_handle().cast()) } == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "SetEndOfFile"));            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void DeviceIoControlSetSparse(long handle)`: FSCTL_SET_SPARSE.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "DeviceIoControlSetSparse",
        "(J)V",
        |ctx, args| {
            const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
            let file = win32_handle_file(ctx, args, "DeviceIoControlSetSparse")?;
            let mut returned = 0u32;
            // SAFETY: live duplicate handle; no buffers.
            let ok = unsafe {
                DeviceIoControl(
                    file.as_raw_handle().cast(),
                    FSCTL_SET_SPARSE,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "DeviceIoControlSetSparse"));            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void DeviceIoControlGetReparsePoint(long handle,
    // long bufferAddress, int bufferSize)`: FSCTL_GET_REPARSE_POINT into the
    // caller's `NativeBuffer`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "DeviceIoControlGetReparsePoint",
        "(JJI)V",
        |ctx, args| {
            const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
            let file = win32_handle_file(ctx, args, "DeviceIoControlGetReparsePoint")?;
            let addr = win32_long_arg(args, 1, "DeviceIoControlGetReparsePoint")?;
            let size = match args.get(2) {
                Some(Value::Int(v)) if *v > 0 => *v as usize,
                _ => 0,
            };
            let mut buf = vec![0u8; size];
            let mut returned = 0u32;
            // SAFETY: live duplicate handle; `buf` is `size` bytes and that
            // length is passed.
            let ok = unsafe {
                DeviceIoControl(
                    file.as_raw_handle().cast(),
                    FSCTL_GET_REPARSE_POINT,
                    std::ptr::null_mut(),
                    0,
                    buf.as_mut_ptr().cast(),
                    size as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "DeviceIoControlGetReparsePoint"));
            }
            let n = (returned as usize).min(size);
            let _ = ctx.copy_to_native_memory(addr, &buf[..n]);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native String GetFinalPathNameByHandle(long handle)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetFinalPathNameByHandle",
        "(J)Ljava/lang/String;",
        |ctx, args| {
            let file = win32_handle_file(ctx, args, "GetFinalPathNameByHandle")?;
            let mut buf = vec![0u16; 1024];
            // SAFETY: live duplicate handle; `buf` length passed.
            let mut len = unsafe {
                GetFinalPathNameByHandleW(file.as_raw_handle().cast(), buf.as_mut_ptr(), 1024, 0)
            } as usize;
            if len > 1024 {
                buf = vec![0u16; len + 1];
                len = unsafe {
                    GetFinalPathNameByHandleW(
                        file.as_raw_handle().cast(),
                        buf.as_mut_ptr(),
                        buf.len() as u32,
                        0,
                    )
                } as usize;
            }
            if len == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "GetFinalPathNameByHandle"));
            }
            let s = ctx.create_string(&String::from_utf16_lossy(&buf[..len.min(buf.len())]));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

/// Set (or clear) a path's read-only bit.
///
/// Split out of `dos_view_set_read_only` so `FileSystemProvider.setAttribute`
/// — which is handed a `Path` and an attribute *name*, never a view object —
/// changes the file exactly the way the view does. Two spellings of
/// "`dos:readonly` is now true" that can drift apart is the
/// `two-caches-one-invalidation-hook` shape.
pub fn set_read_only_on_path(path: &str, on: bool) -> std::io::Result<()> {
    let meta = std::fs::metadata(path)?;
    let mut perms = meta.permissions();
    perms.set_readonly(on);
    std::fs::set_permissions(path, perms)
}

#[cfg(windows)]
pub fn set_dos_flag_on_path(path: &str, flag: u32, on: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    extern "system" {
        fn SetFileAttributesW(lp_file_name: *const u16, dw_file_attributes: u32) -> i32;
    }
    let meta = std::fs::metadata(path)?;
    let mut attrs = meta.file_attributes();
    if on {
        attrs |= flag;
    } else {
        attrs &= !flag;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if unsafe { SetFileAttributesW(wide.as_ptr(), attrs) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Non-Windows hosts have no DOS hidden/system/archive bits. Clearing one is
/// trivially satisfied; SETTING one is refused out loud rather than pretended.
/// `Ok(false)` means "refuse" — the caller turns that into the
/// `UnsupportedOperationException` its own surface owes.
#[cfg(not(windows))]
pub fn set_dos_flag_on_path_supported(path: &str, on: bool) -> std::io::Result<bool> {
    // Validate the file the way the real view does, whichever branch we take.
    std::fs::metadata(path)?;
    Ok(!on)
}

pub fn dos_view_set_read_only(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let on = attr_view_flag_arg(args);
    let path = attr_view_path(ctx, this);
    set_read_only_on_path(&path, on).map_err(|e| p57_io_error(&e))?;
    Ok(None)
}

#[cfg(windows)]
pub fn dos_view_set_flag(ctx: &mut dyn NativeContext, args: &[Value], flag: u32) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let on = attr_view_flag_arg(args);
    let path = attr_view_path(ctx, this);
    set_dos_flag_on_path(&path, flag, on).map_err(|e| RuntimeError::IOException {
        message: format!("SetFileAttributes failed for {path}: {e}"),
    })?;
    Ok(None)
}

#[cfg(not(windows))]
pub fn dos_view_set_flag(ctx: &mut dyn NativeContext, args: &[Value], _flag: u32) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let on = attr_view_flag_arg(args);
    let path = attr_view_path(ctx, this);
    if set_dos_flag_on_path_supported(&path, on).map_err(|e| p57_io_error(&e))? {
        return Ok(None);
    }
    Err(RuntimeError::UnsupportedOperationException {
        message: format!(
            "DOS hidden/system/archive attributes are not settable on this platform: {path}"
        ),
    }
    .into())
}

pub fn dos_view_set_hidden(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dos_view_set_flag(ctx, args, DOS_ATTR_HIDDEN)
}

pub fn dos_view_set_system(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dos_view_set_flag(ctx, args, DOS_ATTR_SYSTEM)
}

pub fn dos_view_set_archive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    dos_view_set_flag(ctx, args, DOS_ATTR_ARCHIVE)
}

/// Backing path recorded in slot 5 of a synthetic `DosFileAttributes` (see
/// `DosFileAttributeView.readAttributes`). Empty when the object predates the
/// 6-slot layout, in which case every flag read below answers `false` — the
/// old hardcoded result.
pub fn dos_attrs_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    if ctx.object_num_fields(this) <= 5 {
        return String::new();
    }
    match ctx.get_field(this, 5) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

#[cfg(windows)]
pub fn dos_attributes_word(path: &str) -> i32 {
    use std::os::windows::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| m.file_attributes() as i32)
        .unwrap_or(0)
}

#[cfg(not(windows))]
pub fn dos_attributes_word(_path: &str) -> i32 {
    0
}

#[cfg(windows)]
pub fn dos_attr_flag(path: &str, flag: u32) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| m.file_attributes() & flag != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn dos_attr_flag(path: &str, flag: u32) -> bool {
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
pub fn file_store_space(ctx: &mut dyn NativeContext, args: &[Value], which: u8) -> i64 {
    let path = match args.first() {
        Some(Value::Object(Some(this))) => {
            let base = file_store_base(ctx, *this);
            match ctx.get_field(*this, base) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),            }        }
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
pub fn copy_options_replace_existing(
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

/// The first `CopyOption` that `Files.copy(InputStream, Path, CopyOption...)`
/// does not support, as its own `toString()`.
///
/// JDK 25's body refuses **every** option but `REPLACE_EXISTING`:
///
/// ```java
/// for (CopyOption opt: options) {
///     if (opt == StandardCopyOption.REPLACE_EXISTING) replaceExisting = true;
///     else throw new UnsupportedOperationException(opt + " not supported");
/// }
/// ```
///
/// Reading only `REPLACE_EXISTING` and ignoring the rest is the same defect as
/// an option scanner that reads only `nofollow`: an option the caller passed to
/// change the behaviour was taken as consent and then dropped. In particular
/// `Files.copy(in, target, NOFOLLOW_LINKS)` is the one `NOFOLLOW_LINKS`
/// assertion that needs no symlink and no privilege, so it is the only such
/// assertion that can run on Windows.
pub fn copy_options_unsupported(
    ctx: &mut dyn NativeContext,
    opts: Option<&Value>,
) -> Option<String> {
    let arr = match opts {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    // Pin: the `toString()` probe re-enters Java and a moving young GC there
    // would relocate the option array (native stale-local family).
    let pin = ctx.pin_native_root(arr);
    let len = ctx.array_length(arr);
    let mut bad: Option<String> = None;
    for i in 0..len {
        let arr_cur = ctx.read_native_pin(pin, arr);
        let Value::Object(Some(opt)) = ctx.get_array_element(arr_cur, i) else {
            continue;
        };
        let Ok(Some(Value::Object(Some(s)))) =
            ctx.invoke_virtual(opt, "toString", "()Ljava/lang/String;", &[])
        else {
            continue;
        };
        let name = ctx.read_string(s).unwrap_or_default();
        if !name.contains("REPLACE_EXISTING") {
            bad = Some(name);
            break;
        }
    }
    ctx.unpin_native_roots(pin);
    bad
}

/// The `FS_ACCESS_*` bits named by a `java.nio.file.AccessMode[]` argument.
///
/// An empty (or absent) array is the JDK's "existence only" request, which the
/// caller has already answered — so this returns an empty vector for it rather
/// than defaulting to some mode the caller never asked about.
pub fn access_modes_requested(
    ctx: &mut dyn NativeContext,
    modes: Option<&Value>,
) -> Vec<i32> {
    let arr = match modes {
        Some(Value::Object(Some(a))) => *a,
        _ => return Vec::new(),
    };
    // Pin: the `toString()` probe re-enters Java and a moving young GC there
    // would relocate the mode array (native stale-local family) — the same
    // hazard `copy_options_replace_existing` guards against.
    let pin = ctx.pin_native_root(arr);
    let len = ctx.array_length(arr);
    let mut out = Vec::new();
    for i in 0..len {
        let arr_cur = ctx.read_native_pin(pin, arr);
        let Value::Object(Some(mode)) = ctx.get_array_element(arr_cur, i) else {
            continue;
        };
        let Ok(Some(Value::Object(Some(s)))) =
            ctx.invoke_virtual(mode, "toString", "()Ljava/lang/String;", &[])
        else {
            continue;
        };
        let text = ctx.read_string(s).unwrap_or_default();
        // `AccessMode` is an enum, so `toString()` is the constant name.
        if text.contains("EXECUTE") {
            out.push(FS_ACCESS_EXECUTE);
        } else if text.contains("WRITE") {
            out.push(FS_ACCESS_WRITE);
        } else if text.contains("READ") {
            out.push(FS_ACCESS_READ);
        }
    }
    ctx.unpin_native_roots(pin);
    out
}

/// Build a REAL `java/nio/file/FileAlreadyExistsException` (same approach as
/// the `Files.move` native): the exception is caught by real library bytecode
/// and its `getFile()` may be read by real `Throwable` formatting, so it needs
/// genuine field layout rather than a guessed synthetic one.
///
/// Round 15 wave 3, lane jdkonly2: one builder with
/// [`p57_file_already_exists_synthetic`] ([`p57_path_exception`]). This body
/// was `new_object` + an `invoke` of `<init>` whose failure it ignored, so a
/// constructor that threw handed back a half-built exception; the shared
/// builder falls back to the minted carrier plus the shadow `Throwable`
/// constructor instead. The flat `IOException` the old body raised when the
/// class did not resolve stays the answer when not even the mint can be built.
pub fn p57_file_already_exists(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> cratonvm_types::error::MethodCallFailed {
    match p57_file_already_exists_synthetic(ctx, path) {
        Ok(thrown) => thrown,
        // Round 15 wave 4, lane jdkonly4 (JO3-2): the constructor's own
        // throwable (`construct_or_mint` propagates it) is the answer.
        Err(thrown @ MethodCallFailed::ExceptionThrown(_)) => thrown,
        Err(_) => RuntimeError::IOException {
            message: format!("FileAlreadyExistsException: {path}"),
        }
        .into(),
    }
}

/// The attribute-view names this VM's default `FileSystem` actually supports,
/// per platform. Single source of truth shared by
/// `FileSystem.supportedFileAttributeViews()` and
/// `FileStore.supportsFileAttributeView(...)`, which previously disagreed —
/// the latter answered `true` for every view name ever asked about.
pub fn supported_attribute_view_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["basic", "dos", "acl", "owner", "user"]
    } else {
        &["owner", "dos", "basic", "posix", "user", "unix"]
    }
}

/// Map a `FileAttributeView` subinterface name (binary or internal form) to
/// the short view name `supportedFileAttributeViews()` reports.
pub fn attribute_view_short_name(class_name: &str) -> Option<&'static str> {
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

/// The concrete `FileStore` HotSpot 25 builds for the default file system,
/// per platform. MEASURED (`regression-suite/probes/W4Abstract.java`, oracle
/// column, Linux): `Files.getFileStore(f).getClass()` is
/// `sun.nio.fs.LinuxFileStore`, where this VM answered the ABSTRACT
/// `java.nio.file.FileStore` -- a receiver `new` cannot legally produce
/// (JVMS 6.5). Ordered; `alloc_concrete` takes the first entry that is present
/// AND instantiable, so the abstract `UnixFileStore` parent is safe to keep as
/// a last-resort entry.
/// The concrete `FileSystemProvider` HotSpot 25 builds for the DEFAULT
/// filesystem — `sun.nio.fs.LinuxFileSystemProvider` on Linux,
/// `WindowsFileSystemProvider` on Windows — where this VM answered the ABSTRACT
/// `java.nio.file.spi.FileSystemProvider` itself.
///
/// **This was the last fabricated-abstract receiver in `java.nio.file`**, and
/// the one L4 §4.4 nominated out of its lane: `Files.probeContentType`'s real
/// bytecode reaches `DefaultFileTypeDetector.create()`, which calls
/// `getFileTypeDetector()` on `DefaultFileSystemProvider.instance()`. The
/// abstract base declares no such method, so the call died with
///
/// ```text
/// NoSuchMethodError: java.nio.file.spi.FileTypeDetector
///                    java.nio.file.spi.FileSystemProvider.getFileTypeDetector()
/// ```
///
/// and took a whole probe run with it. Same shape and the same two halves as
/// `FILE_STORE_IMPLS` and `DIR_STREAM_IMPLS`: mint the concrete class AND
/// mirror the registrations onto it, because dispatch keys on the receiver's
/// runtime class (`H11-1`).
///
/// # History: blocked 2026-09-10, closed 2026-09-16
///
/// Both halves were written, built and run on 2026-09-10. The result then was
/// that a CONCRETE provider needs a CONCRETE PATH first, measured as:
///
/// ```text
/// Files.isExecutable(p)
///   at sun/nio/fs/UnixPath.toUnixPath(UnixPath.java:177)
///   at sun/nio/fs/UnixFileSystemProvider.isExecutable(UnixFileSystemProvider.java:347)
///   -> java.nio.file.ProviderMismatchException
/// ```
///
/// Mirroring moves the rows this file REGISTERS. It cannot move the ones it
/// does not: `isExecutable` has no native here, so with a concrete receiver the
/// real `UnixFileSystemProvider.isExecutable` bytecode runs, and its first act
/// is `UnixPath.toUnixPath(obj)` — an `instanceof UnixPath` test. This VM's
/// Paths were stamped with the INTERFACE `java/nio/file/Path`, so the test
/// failed and every unregistered provider method became a
/// `ProviderMismatchException`. `L4FilesSweep` went from 0 to 172 differing
/// lines, dead at row 226 of 395.
///
/// **Path first, provider second** — `p57_alloc_path`/`p57_alloc_path_raw`
/// (`636ef84ed`) now mint through `concrete_receiver::alloc_concrete` against
/// `path_layout::impl_class()`, with ~100 registrations mirrored onto the
/// concrete `UnixPath`/`WindowsPath` class. `p57_alloc_provider` mints the
/// concrete class as of this commit — the "three-line edit" the previous
/// version of this doc predicted, now that its precondition is closed.
///
/// The scheme's private slot landed on 2026-09-10 anyway
/// (`file_system_provider_base`), so the layout half of this change was
/// already in place.
pub const FILE_SYSTEM_PROVIDER_IMPLS: &[&str] = &[
    "sun/nio/fs/LinuxFileSystemProvider",
    "sun/nio/fs/MacOSXFileSystemProvider",
    "sun/nio/fs/BsdFileSystemProvider",
    "sun/nio/fs/AixFileSystemProvider",
    "sun/nio/fs/SolarisFileSystemProvider",
    "sun/nio/fs/WindowsFileSystemProvider",
    "sun/nio/fs/UnixFileSystemProvider",
];
/// How many private slots a `FileSystemProvider` this crate mints carries: one,
/// the scheme String. The real classes answer `getScheme()` from a constant and
/// declare no field for it, so it has to be a private slot ABOVE their layout —
/// writing it at 0 put it where `UnixFileSystemProvider.theFileSystem` lives.
pub const FILE_SYSTEM_PROVIDER_SLOTS: usize = 1;

/// Where a minted `FileSystemProvider`'s private slot starts on `this`.
///
/// `&dyn`, not `&mut dyn`, and that is load-bearing rather than tidiness: it
/// makes reaching for `<clinit>` or any Java re-entry while resolving a
/// private-slot base a COMPILE ERROR. See [`file_store_base`] for the full
/// account of what that guard is worth.
pub fn file_system_provider_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    cratonvm_native_api::appended_slots::base_for_object(ctx, this, FILE_SYSTEM_PROVIDER_SLOTS)
}

/// The scheme a provider [`p57_alloc_provider`] minted against the ABSTRACT
/// `java/nio/file/spi/FileSystemProvider` serves (`"jar"` / `"jrt"`), read from
/// its private slot; `None` for every other provider (the platform's concrete
/// `file` provider, a JDK-built one, a slot that does not hold a string).
/// Reads only, allocates nothing. For `getClass()`'s per-object display
/// (`lib.rs` `GetClassDisplay::FileSystemMount`; nio21 wave 14, lane L3).
pub fn p57_minted_provider_scheme(
    ctx: &dyn NativeContext,
    provider: ObjectRef,
) -> Option<String> {
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(provider))?;
    if name.as_ref() != "java/nio/file/spi/FileSystemProvider" {
        return None;
    }
    let base = file_system_provider_base(ctx, provider);
    if ctx.object_num_fields(provider) <= base {
        return None;
    }
    match ctx.get_field(provider, base) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

pub const FILE_STORE_IMPLS: &[&str] = &[
    "sun/nio/fs/LinuxFileStore",
    "sun/nio/fs/MacOSXFileStore",
    "sun/nio/fs/BsdFileStore",
    "sun/nio/fs/SolarisFileStore",
    "sun/nio/fs/WindowsFileStore",
    "sun/nio/fs/UnixFileStore",
];
/// How many private slots a `FileStore` this crate mints carries: slot 0, the
/// backing path string.
pub const FILE_STORE_SLOTS: usize = 1;

/// Where a minted `FileStore`'s private slot starts on `this`.
///
/// `&dyn`, not `&mut dyn`, and that is load-bearing rather than tidiness: it
/// makes reaching for `<clinit>` or any Java re-entry while resolving a
/// private-slot base a COMPILE ERROR. (Scope, stated exactly in
/// `appended_slots::base_for_class_id`: `&dyn` blocks every `&mut self` method,
/// which is where `<clinit>` and re-entry live, but NOT a `&self` method using
/// interior mutability.) Until 2026-09-07 this path reached `ensure_class_initialized`
/// and therefore `<clinit>`, so an ordinary private field read was a Java
/// re-entry that could move — or under the generational young sweep zero — every
/// unpinned `ObjectRef` its caller was holding. Widening this back to `&mut`
/// would silently make that possible again; the borrow checker is the only
/// guard that survives a reader who has not read this comment.
pub fn file_store_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    cratonvm_native_api::appended_slots::base_for_object(ctx, this, FILE_STORE_SLOTS)
}

/// The concrete `DirectoryStream` HotSpot 25 builds. MEASURED, same probe:
/// `Files.newDirectoryStream(dir).getClass()` is
/// `sun.nio.fs.UnixSecureDirectoryStream` on Linux, where this VM answered the
/// INTERFACE `java.nio.file.DirectoryStream`.
pub const DIR_STREAM_IMPLS: &[&str] = &[
    "sun/nio/fs/UnixSecureDirectoryStream",
    "sun/nio/fs/UnixDirectoryStream",
    "sun/nio/fs/WindowsDirectoryStream",
];
/// The private map: slot 0 = the materialised `Object[]` of `Path`s, slot 1 =
/// the closed flag, slot 2 = the "an Iterator has already been handed out"
/// latch.
pub const DIR_STREAM_SLOTS: usize = 3;

/// Where a minted `DirectoryStream`'s private map starts on `this`.
///
/// `&dyn`, not `&mut dyn`, and that is load-bearing rather than tidiness: it
/// makes reaching for `<clinit>` or any Java re-entry while resolving a
/// private-slot base a COMPILE ERROR. (Scope, stated exactly in
/// `appended_slots::base_for_class_id`: `&dyn` blocks every `&mut self` method,
/// which is where `<clinit>` and re-entry live, but NOT a `&self` method using
/// interior mutability.) Until 2026-09-07 this path reached `ensure_class_initialized`
/// and therefore `<clinit>`, so an ordinary private field read was a Java
/// re-entry that could move — or under the generational young sweep zero — every
/// unpinned `ObjectRef` its caller was holding. Widening this back to `&mut`
/// would silently make that possible again; the borrow checker is the only
/// guard that survives a reader who has not read this comment.
pub fn dir_stream_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    cratonvm_native_api::appended_slots::base_for_object(ctx, this, DIR_STREAM_SLOTS)
}

/// JIT round 20, lane walkcompat
/// (`docs/feature-designs/keystore-mldsa-mlkem.md`):
/// is `this` a `DirectoryStream` the JDK's own provider BUILT (its constructor
/// ran), rather than one this file minted? A real `UnixDirectoryStream` /
/// `WindowsDirectoryStream` has its `dir` set and a real
/// `UnixSecureDirectoryStream` its `ds`; the mint
/// (`newDirectoryStream` above) sets neither (only `closeLock` / `isOpen`).
/// Pure reads.
pub fn dir_stream_is_jdk_built(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    ["ds", "dir"]
        .iter()
        .any(|f| matches!(ctx.get_field_by_name(this, f), Value::Object(Some(_))))
}

/// The `DirectoryStream` carrier rows are `Intrinsic`s under `--compatible`
/// (`a424fa7e46`: the minted stream has no `ds` / `dfd` for the JDK bodies),
/// so they also won over a stream the JDK's own provider built --
/// `platform_provider_lists_host_dir` hands `UnixFileSystemProvider`'s real
/// `newDirectoryStream` result back for a host directory -- and read their
/// private listing slots off an object that has none: every `Files.list` /
/// `newDirectoryStream` iteration answered `[]` (`R16CollectionsDirSpl
/// --compatible`). Such a receiver runs its own JDK body
/// (`invoke_virtual_bytecode_only`, which skips the natives, so the row does
/// not re-enter itself). `None`: a minted carrier, the row answers.
pub fn ds_yield_to_jdk_built(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return None;
    };
    if !dir_stream_is_jdk_built(ctx, this) {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(this, method, descriptor, &args[1..]))
}

/// Allocate a synthetic `java/nio/file/FileStore` for `path`. Field 0 holds
/// the store's `name()` — the drive root on Windows (`C:\`), or `/` on
/// Unix — since real JDK FileStore names are the mount point, not the
/// queried path itself.
pub fn p57_alloc_file_store(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // Minted AS the concrete per-platform `sun.nio.fs.*FileStore`, not as the
    // ABSTRACT `java.nio.file.FileStore` (JVMS 6.5; see `FILE_STORE_IMPLS`).
    // The private slot moves above whatever that class declares, through
    // `file_store_base`, which every accessor in the `fs_store` registrar also
    // calls -- so the allocator and the readers cannot disagree.
    let minted = cratonvm_native_io::concrete_receiver::alloc_concrete(
        ctx,
        FILE_STORE_IMPLS,
        "java/nio/file/FileStore",
        FILE_STORE_SLOTS,
    );
    let store = minted.obj;
    let store_base = minted.base;
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
    ctx.set_field(store, store_base, Value::Object(Some(s)));
    ctx.unpin_native_roots(store_pin);
    Ok(store)
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
// ---------------------------------------------------------------------------
// FileSystem.getPathMatcher / PathMatcher.matches — the glob and regex syntaxes
// ---------------------------------------------------------------------------

/// Field 0 of the one-field synthetic `java/nio/file/PathMatcher`: the compiled
/// `java.util.regex.Pattern`.
pub const P57_MATCHER_PATTERN_FIELD: usize = 0;

/// `Pattern.CASE_INSENSITIVE | Pattern.UNICODE_CASE` = 2 | 64 = 66, the flag
/// word `sun.nio.fs.WindowsFileSystem.getPathMatcher` compiles with — for the
/// `regex:` syntax as well as `glob:`. MEASURED on HotSpot 25.0.3+9 (Windows):
/// `regex:A.TXT` matches `a.txt` and `regex:(?-i)A\.TXT` does not, which is
/// only possible if the flag is on the compiled Pattern; `regex:Ä` matching
/// `ä` is the `UNICODE_CASE` half.
pub const P57_MATCHER_FLAGS: i32 = 0x02 | 0x40;

/// `sun.nio.fs.Globs.globMetaChars`, verbatim.
pub const P57_GLOB_META: &str = "\\*?[{";

/// `sun.nio.fs.Globs.regexMetaChars`, verbatim. Note what is ABSENT and must
/// stay absent: `*`, `?`, `-`, `}`, `,`, `&`, `:`, `/`, `\` — each is either
/// handled by its own case below or is not a regex metacharacter outside a
/// character class. Adding any of them would escape a character HotSpot leaves
/// bare and change the compiled pattern.
pub const P57_REGEX_META: &str = ".^$+{[]|()";

#[inline]
pub fn p57_is_glob_meta(c: u16) -> bool {
    c < 0x80 && P57_GLOB_META.as_bytes().contains(&(c as u8))
}

#[inline]
pub fn p57_is_regex_meta(c: u16) -> bool {
    c < 0x80 && P57_REGEX_META.as_bytes().contains(&(c as u8))
}

/// `sun.nio.fs.Globs.next(glob, i)`: the unit at `i`, or `EOL` (0) past the end.
#[inline]
pub fn p57_glob_next(g: &[u16], i: usize) -> u16 {
    if i < g.len() {
        g[i]
    } else {
        0
    }
}

#[inline]
pub fn p57_push_ascii(out: &mut Vec<u16>, s: &str) {
    out.extend(s.as_bytes().iter().map(|b| u16::from(*b)));
}

/// `sun.nio.fs.Globs.toRegexPattern(globPattern, isDos)`, ported statement for
/// statement from the JDK 25.0.3+9 source (`lib/src.zip`,
/// `java.base/sun/nio/fs/Globs.java`).
///
/// `Err((desc, index))` is the argument pair of the `PatternSyntaxException`
/// the JDK raises; [`p57_throw_pattern_syntax`] turns it into the real Java
/// object so `getMessage()` — which appends `" near index N"`, the pattern and
/// a caret line, using the platform line separator — is composed by the JDK
/// itself rather than reproduced here.
///
/// Operates on UTF-16 code units, not `char`s, so every index in an error
/// message is the same index `String.charAt` would have produced. The six
/// refusals, all MEASURED on HotSpot and all with their quoting exactly as
/// written (two of them carry an unbalanced apostrophe, which is not a typo
/// here): `No character to escape`, `Explicit 'name separator' in class`,
/// `Missing ']`, `Invalid range`, `Cannot nest groups`, `Missing '}`.
///
/// The Windows branch (`is_dos`) is the one this VM takes. Its consequences,
/// each MEASURED: `*` is `[^\\]*` and `?` is `[^\\]`, so neither crosses a
/// separator; `**` is `.*` and does; a `/` in the PATTERN is the separator
/// while a lone `\` is the ESCAPE character (so the glob `sub\a.txt` matches
/// the file name `suba.txt`, not a directory — the single likeliest place a
/// port goes wrong); and a class is emitted as `[[^\\]&&[...]]`, an
/// intersection that excludes the separator, which is why the Java regex engine
/// has to do the matching and a Rust regex crate cannot.
pub fn p57_globs_to_regex(glob: &[u16], is_dos: bool) -> Result<Vec<u16>, (&'static str, usize)> {
    let mut in_group = false;
    let mut regex: Vec<u16> = Vec::with_capacity(glob.len() * 2 + 2);
    p57_push_ascii(&mut regex, "^");

    let mut i = 0usize;
    while i < glob.len() {
        let c = glob[i];
        i += 1;
        // Non-ASCII cannot be any of the cases below; 0xFF is a sentinel that
        // falls through to the default arm, which reads `c` and not this.
        let cb = if c < 0x80 { c as u8 } else { 0xFF };
        match cb {
            b'\\' => {
                if i == glob.len() {
                    return Err(("No character to escape", i - 1));
                }
                let next = glob[i];
                i += 1;
                if p57_is_glob_meta(next) || p57_is_regex_meta(next) {
                    p57_push_ascii(&mut regex, "\\");
                }
                regex.push(next);
            }
            b'/' => {
                if is_dos {
                    p57_push_ascii(&mut regex, "\\\\");
                } else {
                    regex.push(c);
                }
            }
            b'[' => {
                // Do not match the name separator inside a class.
                if is_dos {
                    p57_push_ascii(&mut regex, "[[^\\\\]&&[");
                } else {
                    p57_push_ascii(&mut regex, "[[^/]&&[");
                }
                if p57_glob_next(glob, i) == u16::from(b'^') {
                    // `^` is a LITERAL in a glob class, never negation — it is
                    // escaped through. `[!…]` is the negation form.
                    p57_push_ascii(&mut regex, "\\^");
                    i += 1;
                } else {
                    if p57_glob_next(glob, i) == u16::from(b'!') {
                        p57_push_ascii(&mut regex, "^");
                        i += 1;
                    }
                    // A hyphen is allowed at the start.
                    if p57_glob_next(glob, i) == u16::from(b'-') {
                        p57_push_ascii(&mut regex, "-");
                        i += 1;
                    }
                }
                let mut has_range_start = false;
                let mut last: u16 = 0;
                // Seeded with `[` exactly as the JDK's `c` is: an EMPTY class
                // body leaves it unchanged and the `!= ']'` test below then
                // reports `Missing '] near index 0` for the glob `[`.
                let mut cc = c;
                while i < glob.len() {
                    cc = glob[i];
                    i += 1;
                    if cc == u16::from(b']') {
                        break;
                    }
                    if cc == u16::from(b'/') || (is_dos && cc == u16::from(b'\\')) {
                        return Err(("Explicit 'name separator' in class", i - 1));
                    }
                    // Escape `\`, `[` or `&&` for the regex class.
                    if cc == u16::from(b'\\')
                        || cc == u16::from(b'[')
                        || (cc == u16::from(b'&') && p57_glob_next(glob, i) == u16::from(b'&'))
                    {
                        p57_push_ascii(&mut regex, "\\");
                    }
                    regex.push(cc);

                    if cc == u16::from(b'-') {
                        if !has_range_start {
                            return Err(("Invalid range", i - 1));
                        }
                        cc = p57_glob_next(glob, i);
                        i += 1;
                        if cc == 0 || cc == u16::from(b']') {
                            break;
                        }
                        if cc < last {
                            return Err(("Invalid range", i.saturating_sub(3)));
                        }
                        regex.push(cc);
                        has_range_start = false;
                    } else {
                        has_range_start = true;
                        last = cc;
                    }
                }
                if cc != u16::from(b']') {
                    return Err(("Missing ']", i - 1));
                }
                p57_push_ascii(&mut regex, "]]");
            }
            b'{' => {
                if in_group {
                    return Err(("Cannot nest groups", i - 1));
                }
                p57_push_ascii(&mut regex, "(?:(?:");
                in_group = true;
            }
            b'}' => {
                if in_group {
                    p57_push_ascii(&mut regex, "))");
                    in_group = false;
                } else {
                    // `}` is not in `regexMetaChars`, so a stray one is a plain
                    // literal: the glob `a}b` matches the file name `a}b`.
                    regex.push(c);
                }
            }
            b',' => {
                if in_group {
                    p57_push_ascii(&mut regex, ")|(?:");
                } else {
                    regex.push(c);
                }
            }
            b'*' => {
                if p57_glob_next(glob, i) == u16::from(b'*') {
                    // Crosses directory boundaries.
                    p57_push_ascii(&mut regex, ".*");
                    i += 1;
                } else if is_dos {
                    p57_push_ascii(&mut regex, "[^\\\\]*");
                } else {
                    p57_push_ascii(&mut regex, "[^/]*");
                }
            }
            b'?' => {
                if is_dos {
                    p57_push_ascii(&mut regex, "[^\\\\]");
                } else {
                    p57_push_ascii(&mut regex, "[^/]");
                }
            }
            _ => {
                if p57_is_regex_meta(c) {
                    p57_push_ascii(&mut regex, "\\");
                }
                regex.push(c);
            }
        }
    }

    if in_group {
        return Err(("Missing '}", i - 1));
    }
    p57_push_ascii(&mut regex, "$");
    Ok(regex)
}

/// The `java.util.regex.PatternSyntaxException` the glob translator refuses
/// with, built through its real `(String desc, String regex, int index)`
/// constructor so `getMessage()` is composed by the JDK.
///
/// Reproducing that message here would be a transcription with three moving
/// parts (the `" near index N"` suffix, the platform line separator — CRLF on
/// this host — and a caret line that is OMITTED when `index == pattern
/// .length()`); letting the real class compose it removes all three.
pub fn p57_throw_pattern_syntax(
    ctx: &mut dyn NativeContext,
    desc: &str,
    pattern: &str,
    index: usize,
) -> MethodCallFailed {
    let d = ctx.create_string(desc);
    // `create_string` allocates and can move the first string.
    let d_pin = ctx.pin_native_root(d);
    let p = ctx.create_string(pattern);
    let d = ctx.read_native_pin(d_pin, d);
    ctx.unpin_native_roots(d_pin);
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/util/regex/PatternSyntaxException",
        "(Ljava/lang/String;Ljava/lang/String;I)V",
        &[
            Value::Object(Some(d)),
            Value::Object(Some(p)),
            Value::Int(index as i32),
        ],
        || {
            RuntimeError::IllegalArgumentException {
                message: format!("{desc} near index {index}"),
            }
            .into()
        },
    )
}

/// `throw new IllegalArgumentException()` — the BARE one
/// `FileSystem.getPathMatcher` raises when the argument carries no syntax
/// prefix. Its `getMessage()` is **null**, which
/// `RuntimeError::IllegalArgumentException { message: String }` cannot express,
/// so the object is built directly. MEASURED: HotSpot reports a null message
/// for `"*.txt"`, `""`, `":"` and `":glob"` alike.
pub fn p57_bare_illegal_argument(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    match ctx.new_object_initialized("java/lang/IllegalArgumentException", "()V", &[]) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into(),
    }
}

pub fn p57_default_filesystem_singleton(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
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
        // Once minted, the singleton is never replaced: `FileSystems.getDefault()`
        // is a process-lifetime identity (`getDefault() == getDefault()`,
        // `theFileSystem() == getDefault()`), so a later "upgrade" would split
        // it. A 2026-09-22 revision did swap a synthetic-shaped cache for a
        // fresh concrete mint here, on the theory that the H2 in-process-javac
        // `NoSuchMethodError` cluster came from THIS object; it did not -- the
        // abstract `FileSystem` in that failure was the `jrt:` one
        // (`fs-cluster-needtoresolveagainstdefaultdirectory-FIXED-20260923.md`),
        // and the swap was removed with that finding.
        if let Value::Object(Some(fs)) = ctx.get_static_field(cid, idx) {
            return Ok(fs);
        }
        let fs = p57_alloc_default_filesystem(ctx)?;
        ctx.set_static_field(cid, idx, Value::Object(Some(fs)));
        return Ok(fs);
    }
    Ok(p57_alloc_default_filesystem(ctx)?)
}

/// The URI scheme this synthetic FileSystem serves: "jrt" / "jar" / "file".
/// Reads only, allocates nothing, so callers may use it before pinning.
/// Width-guarded (`p57_fs_is_synthetic_shaped`): its one caller already
/// guards this the same way, but a receiver narrower than `P57_FS_SLOTS`
/// (this VM's own concrete default FileSystem) is never virtual regardless,
/// so this stays correct standalone too.
pub fn p57_fs_scheme(ctx: &dyn NativeContext, fs: ObjectRef) -> &'static str {
    if !p57_fs_is_synthetic_shaped(ctx, fs) {
        return "file";
    }
    if matches!(ctx.get_field(fs, P57_FS_JRT_FIELD), Value::Object(Some(_))) {
        "jrt"
    } else if matches!(ctx.get_field(fs, P57_FS_JAR_FIELD), Value::Object(Some(_))) {
        "jar"
    } else {
        "file"
    }
}

/// The ONE `FileSystemProvider` object belonging to `fs` — allocated on first
/// use, then stored in `P57_FS_PROVIDER_FIELD` and returned unchanged forever.
///
/// `FileSystem.provider()` and `FileSystemProvider.installedProviders()` both
/// minted a FRESH provider on every call before this helper existed, so even
/// `fs.provider() == fs.provider()` — a self-comparison — was false. HotSpot
/// guarantees a singleton: `WindowsFileSystemProvider` is created exactly once,
/// in `DefaultFileSystemProvider.<clinit>`, and every door returns that object.
///
/// The identity is load-bearing in java.base itself, not just in applications:
/// `sun.nio.ch.UnixDomainSockets.generateTempName()` reads, verbatim,
/// `if (path.getFileSystem().provider() != DefaultFileSystemProvider.instance())`
/// and throws `UnsupportedOperationException` when that holds — a raw `!=`
/// between the two doors this helper unifies. (Deliberately not a fenced code
/// block: rustdoc collects doctests from private items and would try to compile
/// Java as Rust.)
pub fn p57_fs_provider(
    ctx: &mut dyn NativeContext,
    fs: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // SLOT-WIDTH GUARD — do not remove, and do not read slot 3 above it.
    //
    // Only CratonVM's own synthetic `java/nio/file/FileSystem` has
    // P57_FS_SLOTS slots. A REAL `sun.nio.fs.WindowsFileSystem` has exactly
    // THREE instance fields (javap: `provider`, `defaultDirectory`,
    // `defaultRoot`), so `P57_FS_PROVIDER_FIELD` is one past its end, and
    // dispatch has been receiver-aware since 2026-07 — a real FileSystem
    // subtype receiver's `provider()` call routes into this native. An
    // unguarded read here would be an out-of-bounds field read on a real JDK
    // class, which is a genuine defect in this VM and not a cosmetic one.
    //
    // The same width mismatch is also why the scheme sniff must NOT run on a
    // real receiver: `p57_fs_scheme` reads slots 1 and 2 looking for our
    // jar/jrt markers, but on a real `WindowsFileSystem` those slots hold
    // `defaultDirectory` and `defaultRoot` — both non-null Strings — so it
    // would report "jrt" for the platform's own file system. (The
    // per-call-allocating code this helper replaced had exactly that bug, and
    // answered `fs.provider().getScheme()` = "jrt" for a real receiver.)
    // A REAL platform FileSystem (`LinuxFileSystem` is as wide as the synthetic
    // shape, so width cannot tell them apart): its `provider` field is the
    // singleton the constructor stored.
    if !p57_fs_is_synthetic_shaped(ctx, fs) && ctx.object_num_fields(fs) > 0 {
        if let Value::Object(Some(p)) = ctx.get_field_by_name(fs, "provider") {
            return Ok(p);
        }
    }
    if p57_fs_is_synthetic_shaped(ctx, fs) && ctx.object_num_fields(fs) > P57_FS_PROVIDER_FIELD {
        if let Value::Object(Some(p)) = ctx.get_field(fs, P57_FS_PROVIDER_FIELD) {
            return Ok(p);
        }
        let scheme_str = p57_fs_scheme(ctx, fs);
        // Pin `fs` across BOTH allocations below: either can trigger a moving
        // young GC that relocates it, and we still have to write slot 3
        // afterwards (native stale-local family).
        let fs_pin = ctx.pin_native_root(fs);
        let provider = p57_alloc_provider(ctx, scheme_str)?;
        let fs = ctx.read_native_pin(fs_pin, fs);
        ctx.set_field(fs, P57_FS_PROVIDER_FIELD, Value::Object(Some(provider)));
        ctx.unpin_native_roots(fs_pin);
        return Ok(provider);
    }

    // Undersized receiver. As of the 2026-09-17 default-FileSystem carrier
    // fix, this VM's OWN default FileSystem is now one of these — its real
    // `provider` field already holds the singleton `p57_alloc_default_filesystem`
    // wrote at construction time. Read it back rather than re-minting, or
    // `fs.provider() == fs.provider()` goes false the moment the default
    // FileSystem became concrete (see `native-api::filesystem_layout`, and
    // that module's own doc for why jar/jrt FileSystems never reach here —
    // they stay `P57_FS_SLOTS`-wide and are caught by the branch above).
    if let Some(slots) = cratonvm_native_api::filesystem_layout::slots_for(ctx.vm_identity()) {
        if ctx.object_num_fields(fs) == slots.width {
            if let Value::Object(Some(p)) = ctx.get_field(fs, slots.provider) {
                return Ok(p);
            }
        }
    }

    // The only other real receiver that reaches here is the platform
    // default on an image/platform this fix did not resolve a layout for,
    // whose provider on HotSpot IS `DefaultFileSystemProvider.instance()` —
    // so answer with the default singleton rather than minting a per-call
    // object.
    //
    // Terminates: the default filesystem singleton is always either
    // `P57_FS_SLOTS`-wide (caught by the first branch) or this VM's own
    // resolved concrete shape (caught by the branch above), so the
    // recursive call cannot re-enter this one.
    let dfs = p57_default_filesystem_singleton(ctx)?;
    if ctx.object_num_fields(dfs) > P57_FS_PROVIDER_FIELD {
        return p57_fs_provider(ctx, dfs);
    }
    // Last resort only if even the singleton came back undersized in a shape
    // this fix does not recognise. Uncached, so identity is not guaranteed
    // — but never OOB.
    p57_alloc_provider(ctx, "file")
}

/// Allocate a fresh 1-field synthetic `FileSystemProvider` carrying `scheme`.
/// Callers are responsible for caching it; use `p57_fs_provider` unless you
/// specifically need an uncached one (the jar/jrt entries of
/// `installedProviders`).
pub fn p57_alloc_provider(
    ctx: &mut dyn NativeContext,
    scheme_str: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // THE CONCRETE CLASS, as of 2026-09-16 -- the Path half this doc block
    // used to wait on is closed (`p57_alloc_path` / `path_layout.rs`; see
    // that function's own doc), so `L4FilesSweep`'s
    // `UnixPath.toUnixPath`/`WindowsPath.toWindowsPath` `instanceof` check
    // no longer fails against a native-built Path reached through a
    // concrete provider. `alloc_concrete` walks `FILE_SYSTEM_PROVIDER_IMPLS`
    // to the first class this image can load, matching the platform
    // `sun.nio.fs.*FileSystemProvider` the real default provider is.
    //
    // ONLY for `scheme_str == "file"` — `FILE_SYSTEM_PROVIDER_IMPLS` is
    // exclusively `sun.nio.fs.*FileSystemProvider` classes, and every one of
    // them declares its own concrete `getScheme()` returning the LITERAL
    // `"file"`, real bytecode, no field read. Minting a "jar"/"jrt" provider
    // against one of those classes stamps an object whose real `getScheme()`
    // still answers `"file"` — invisible as long as `getScheme()` ran through
    // this file's OWN native at the abstract `FileSystemProvider` level, but
    // under `--real-jdk` the concrete class's real Code wins dispatch, so
    // `FileSystems.newFileSystem(jarUri, ...)`'s real
    // `installedProviders().stream().filter(p -> p.getScheme().equals("jar"))`
    // found nothing and threw `ProviderNotFoundException: Provider "jar" not
    // found` (`PemContentTests` et al., loading PEM/JKS content from a jar
    // via `Resources.addPackage` -> `FileSystems.newFileSystem(URI, Map)`).
    // An empty candidate list makes `alloc_concrete` take its own
    // no-match-found fallback and mint against the ABSTRACT class directly,
    // which has no `Code` for `getScheme()` (or any other `fsp`-registered
    // method below) for real dispatch to prefer over this file's natives.
    let candidates: &[&str] = if scheme_str == "file" {
        FILE_SYSTEM_PROVIDER_IMPLS
    } else {
        &[]
    };
    let provider = cratonvm_native_io::concrete_receiver::alloc_concrete(
        ctx,
        candidates,
        "java/nio/file/spi/FileSystemProvider",
        FILE_SYSTEM_PROVIDER_SLOTS,
    )
    .obj;
    let base = file_system_provider_base(ctx, provider);
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh provider (native stale-local family).
    let provider_pin = ctx.pin_native_root(provider);
    let scheme = ctx.create_string(scheme_str);
    let provider = ctx.read_native_pin(provider_pin, provider);
    ctx.set_field(provider, base, Value::Object(Some(scheme)));
    ctx.unpin_native_roots(provider_pin);
    Ok(provider)
}

/// Write the backward half of the provider/filesystem link real JDK
/// constructors set for free: `sun.nio.fs.{Windows,Unix}FileSystemProvider`
/// each declare a `theFileSystem` field, populated in the PROVIDER's own
/// constructor (`theFileSystem = new WindowsFileSystem(this)`). This VM never
/// runs that constructor — `provider` is fabricated by [`p57_alloc_provider`]
/// and `fs` is minted separately, with only the forward link (`fs.provider =
/// provider`) written by the caller — so `theFileSystem` stayed null.
///
/// Invisible as long as every reader went through this VM's own natives, all
/// of which resolve the default FileSystem through [`p57_default_filesystem_singleton`]
/// rather than through the provider. Real bytecode does not: `Path.of(URI)`
/// on a "file" scheme (JDK 11+) calls `FileSystems.getDefault().provider()
/// .getPath(uri)`, whose real `WindowsFileSystemProvider.getPath`/
/// `UnixFileSystemProvider.getPath` body reads `theFileSystem` directly and
/// hands it to `*PathParser`/`*UriSupport`, which stores it in the new Path's
/// `fs` field. A null `theFileSystem` there means every URI-constructed Path
/// this VM does not intercept with its own `Path.of(URI)`/
/// `FileSystemProvider.getPath(URI)` natives (i.e. every case where
/// `--real-jdk` prefers the real provider's own bytecode) comes back with
/// `getFileSystem() == null` — the exact shape of the NPE at
/// `Files.provider(Files.java:103)` that broke `PathMatchingResourcePatternResolver`
/// and, through it, Spring's classpath component scanning
/// (`defects-and-fixes/fixed-suite-bugs/hibernate/nonpassed-classbyclass-census-20260922-FIXED-20260922.md`).
///
/// By `ClassId`, not by name: `provider` is already minted as its real
/// concrete class (`alloc_concrete` in [`p57_alloc_provider`]), so
/// `resolve_field_index_by_class_id` finds `theFileSystem` on the first try
/// with no ambiguous-name fallback to worry about. A no-op (not an error)
/// when the field cannot be resolved — a synthetic-JDK image, where nothing
/// reads a real field by that name anyway.
pub fn p57_link_provider_to_filesystem(
    ctx: &mut dyn NativeContext,
    provider: ObjectRef,
    fs: ObjectRef,
) {
    let cid = ctx.class_id_of_object(provider);
    if let Some(idx) = ctx.resolve_field_index_by_class_id(cid, "theFileSystem") {
        ctx.set_field(provider, idx, Value::Object(Some(fs)));
    }
}

/// The default filesystem's provider — the object HotSpot calls
/// `DefaultFileSystemProvider.instance()` and `FileSystems.getDefault()
/// .provider()`, and which is element 0 of `installedProviders()`.
pub fn p57_default_provider_singleton(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let fs = p57_default_filesystem_singleton(ctx)?;
    p57_fs_provider(ctx, fs)
}

/// Resolve `(name_slot, ordinal_slot)` for an enum constant's two `Enum`
/// fields — the single place in this file where those slots are decided.
///
/// **Resolved against `java/lang/Enum`, never against the receiver's class.**
/// `resolve_field_index_by_class_id` returns the MOST-DERIVED declaration, and
/// an enum may declare its own field called `name`, which shadows `Enum`'s:
/// Spring Boot's `WebEndpointTest.Infrastructure` does exactly that
/// (`JERSEY("Jersey")`), which made `name()` answer `"Jersey"` instead of
/// `"JERSEY"` and took the whole test class down with a
/// `PreconditionViolationException`. `lang_misc::native_enum_name` carries the
/// full account; this helper exists so no site in this file can reach for a
/// literal or for the receiver again.
///
/// E36-1 N4: the fallback slots used to be a local copy of this file's own
/// (once-inverted) `ENUM_NAME_SLOT`/`ENUM_ORDINAL_SLOT`. `lang_misc`'s pair is
/// now `pub`, so this is the one copy that matters, imported rather
/// than restated.
pub fn enum_name_ordinal_slots(ctx: &mut dyn NativeContext) -> (usize, usize) {
    // `Enum` must be loaded before its layout can be resolved; on the
    // synthetic side this is what materialises the two-field stub model.
    let _ = ctx.ensure_class_initialized("java/lang/Enum");
    let name_slot = ctx
        .resolve_field_index("java/lang/Enum", "name")
        .unwrap_or(crate::lang_misc::ENUM_NAME_SLOT);
    let ordinal_slot = ctx
        .resolve_field_index("java/lang/Enum", "ordinal")
        .unwrap_or(crate::lang_misc::ENUM_ORDINAL_SLOT);
    (name_slot, ordinal_slot)
}

pub fn p57_alloc_enum(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    ordinal: i32,
) -> MethodCallResult {
    // JLS 8.9: an enum's constants are its ONLY instances, so an answer that
    // names one must BE that one -- the object the class's own `<clinit>`
    // published. A fresh copy passes `name()`/`ordinal()` and fails `==`,
    // `HashMap` / `IdentityHashMap` / `Set.of` lookups keyed by the constant
    // (i8-L2, fixed i9-L2). Minting below is only for a class that declares
    // no such static or has not published it yet. Both kinds carry `name` and
    // `ordinal` in `java/lang/Enum`'s slots, so a native reading either reads
    // the same fields.
    if let Some(constant) = crate::lang_system::canonical_enum_constant(ctx, class, name) {
        return Ok(Some(constant));
    }
    let obj = p57_mint_enum(ctx, class, name, ordinal)?;
    Ok(Some(Value::Object(Some(obj))))
}

/// The minting half of [`p57_alloc_enum`]: a fresh constant in
/// `java/lang/Enum`'s `name` / `ordinal` layout, never the published one.
/// Called directly only by a `<clinit>` that is publishing the constants
/// (i10-L2, `FormatStyle`), where there is nothing to look up yet.
pub fn p57_mint_enum(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    ordinal: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // Was two hard-coded literals (`0`/`1`). They happened to be the right way
    // round, which is exactly why they were dangerous: the sibling copy of the
    // same two lines in `posix_file_permission_stub_clinit` was INVERTED and
    // nothing connected the two. One resolver, one order.
    let (name_slot, ordinal_slot) = enum_name_ordinal_slots(ctx);
    let obj = try_alloc_concurrent_synthetic(ctx, class, 2)?;
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh enum (native stale-local family).
    let obj_pin = ctx.pin_native_root(obj);
    let n = ctx.create_string(name);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, name_slot, Value::Object(Some(n)));
    ctx.set_field(obj, ordinal_slot, Value::Int(ordinal));
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

// ---------------------------------------------------------------------------
// java.io.RandomAccessFile — 2-field (fd_id=0 Int, mode=1 Int)
// mode: 0=read-only, 1=read-write
// ---------------------------------------------------------------------------

/// Resolve the real-JDK `fd` FileDescriptor object on a RandomAccessFile, if
/// the loaded class declares the field. Returns `None` for the synthetic
/// 2-slot layout (where slot 0 is the raw `Int` fd_id).
pub fn raf_fd_object(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
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
pub fn raf_set_fd(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    fd: u32,
) -> Result<(), MethodCallFailed> {
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
                let new_fd = try_alloc_concurrent_synthetic(ctx, "java/io/FileDescriptor", 1)?;
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field_by_name(this, "fd", Value::Object(Some(new_fd)));
                ctx.unpin_native_roots(this_pin);
                new_fd
            }
        };
        // Platform spelling (`CRATONVM_FD_FIELDS_PER_PLATFORM`, JIT r23 w1 L6);
        // `raf_get_fd` reads `fd` then `handle`.
        cratonvm_native_io::nio_native::store_descriptor_id(ctx, fd_obj, fd);
        return Ok(());
    }
    ctx.set_field(this, 0, Value::Int(fd as i32));
    Ok(())
}

/// Recover the open RAF fd id. Tries the real-JDK `fd.fd`/`fd.handle` pair
/// first, then falls back to slot 0 for the synthetic 2-slot layout.
pub fn raf_get_fd(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u32> {
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
pub fn raf_open_failure(
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

pub fn register_phase57_random_access_file(r: &mut NativeMethodRegistry) {
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
                raf_set_fd(ctx, this, fd_id)?;
                ctx.set_field(this, 1, Value::Int(0)); // read-only
            } else {
                let fd_id = crate::capability_gate::open_read_write_gated(&*ctx, &path, create)
                    .map_err(raf_open_failure(&path))?;
                raf_set_fd(ctx, this, fd_id)?;
                ctx.set_field(this, 1, Value::Int(1)); // read-write            
}
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
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
                            _ => None,                        },                    };
                    p.unwrap_or_default()                }
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
            raf_set_fd(ctx, this, fd_id)?;
            ctx.set_field(this, 1, Value::Int(if writable { 1 } else { 0 }));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
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
            // `-1` is `RandomAccessFile.read`'s "the end of the file has been
            // reached", and the same method declares "@throws IOException If
            // the first byte cannot be read for any reason other than end of
            // file". Answering EOF for a failure merged the two states the
            // contract exists to keep apart, so a read loop stopped early and
            // its caller kept the truncated result.
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    }, cratonvm_native_api::NativeKind::Bridge);

    // read(byte[], int off, int len) -> int
    r.register(raf, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        if fd_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            // A null buffer: `io_util.c`'s `JNU_ThrowNullPointerException(env, NULL)`
            // (wave 100 L7).
            _ => {
                return Err(
                    cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
                )
            }
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
            // HotSpot's `io_util.c` `readBytes`/`writeBytes`:
            // `IndexOutOfBoundsException` with no message (wave 100 L7; this was an
            // AIOOBE "Array index out of range: n").
            return Err(cratonvm_types::error::RuntimeError::ioobe_no_message().into());
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
            // `-1` is `RandomAccessFile.read`'s "the end of the file has been
            // reached", and the same method declares "@throws IOException If
            // the first byte cannot be read for any reason other than end of
            // file". Answering EOF for a failure merged the two states the
            // contract exists to keep apart, so a read loop stopped early and
            // its caller kept the truncated result.
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    }, cratonvm_native_api::NativeKind::Bridge);

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
            // `-1` is `RandomAccessFile.read`'s "the end of the file has been
            // reached", and the same method declares "@throws IOException If
            // the first byte cannot be read for any reason other than end of
            // file". Answering EOF for a failure merged the two states the
            // contract exists to keep apart, so a read loop stopped early and
            // its caller kept the truncated result.
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    }, cratonvm_native_api::NativeKind::Bridge);

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
            ctx.set_array_element(arr, i, Value::Int(buf[i] as i8 as i32));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // readFully(byte[], int off, int len)
    r.register(raf, "readFully", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            // `readFully` -> `read` -> `io_util.c`: an NPE with no message,
            // never an `IOException: null buffer` (wave 100 L7).
            _ => {
                return Err(RuntimeError::NullPointerException { message: None }.into())
            }
        };
        // BUG nb-phases-late(1): same bounds-check as read([BII) — validate
        // signed off/len before casting to usize to avoid a negative-len
        // sign-extension alloc abort. JDK throws IndexOutOfBoundsException.
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let arr_len = ctx.array_length(arr) as i64;
        if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
            // HotSpot's `io_util.c` `readBytes`/`writeBytes`:
            // `IndexOutOfBoundsException` with no message (wave 100 L7; this was an
            // AIOOBE "Array index out of range: n").
            return Err(cratonvm_types::error::RuntimeError::ioobe_no_message().into());
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
            ctx.set_array_element(arr, off + i, Value::Int(buf[i] as i8 as i32));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

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
    }, cratonvm_native_api::NativeKind::Bridge);

    // write(byte[], int off, int len)
    r.register(raf, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            // A null buffer: `io_util.c`'s `JNU_ThrowNullPointerException(env, NULL)`
            // (wave 100 L7).
            _ => {
                return Err(
                    cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
                )
            }
        };
        // BUG nb-phases-late(1): validate signed off/len before the loop so a
        // negative len cannot sign-extend into a huge `Vec::with_capacity`.
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let arr_len = ctx.array_length(arr) as i64;
        if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
            // HotSpot's `io_util.c` `readBytes`/`writeBytes`:
            // `IndexOutOfBoundsException` with no message (wave 100 L7; this was an
            // AIOOBE "Array index out of range: n").
            return Err(cratonvm_types::error::RuntimeError::ioobe_no_message().into());
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
    }, cratonvm_native_api::NativeKind::Bridge);

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
    }, cratonvm_native_api::NativeKind::Bridge);

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
        // "@throws IOException if pos is less than 0 or if an I/O error
        // occurs" (`RandomAccessFile.seek`) — an `IOException`, not the
        // `IllegalArgumentException` `FileChannel.position(long)` raises for
        // the same input; the two classes genuinely differ here. The
        // `pos.max(0)` this replaces answered a successful seek to the START of
        // the file, so the next read returned the first record rather than the
        // one the caller's (bad) arithmetic had asked for.
        if pos < 0 {
            return Err(RuntimeError::IOException {
                message: format!("Negative seek offset: {pos}"),
            }
            .into());
        }
        ctx.fd_table()
            .rw_seek(fd_id as u32, std::io::SeekFrom::Start(pos as u64))
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // getFilePointer() -> long
    r.register(raf, "getFilePointer", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let pos = ctx.fd_table().rw_position(fd_id as u32).unwrap_or(0);
        Ok(Some(Value::Long(pos as i64)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // length() -> long
    r.register(raf, "length", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let size = ctx.fd_table().file_size(fd_id as u32).unwrap_or(0);
        Ok(Some(Value::Long(size as i64)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // setLength(long newLength)
    r.register(raf, "setLength", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let new_len = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
            _ => 0,
        };
        // `as u64` on a negative length wrapped to an enormous positive one and
        // handed it to `rw_set_length`, so `setLength(-1)` asked the host to
        // grow the file to 16 exabytes; whether that failed or succeeded, the
        // answer the caller got back was not the refusal the contract names.
        // `RandomAccessFile.setLength`: "@throws IOException If an I/O error
        // occurs", and HotSpot's `SetFileLength`/`ftruncate` rejects a negative
        // length as one.
        if new_len < 0 {
            return Err(RuntimeError::IOException {
                message: format!("Negative file length: {new_len}"),
            }
            .into());
        }
        ctx.fd_table()
            .rw_set_length(fd_id as u32, new_len as u64)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

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
                ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));            } else {
                ctx.set_field(this, 0, Value::Int(-1));            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- DataInput interface methods ---
    r.register(raf, "readInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 4];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(i32::from_be_bytes(buf))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 8];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Long(i64::from_be_bytes(buf))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 2];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(i16::from_be_bytes(buf) as i32)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 2];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(u16::from_be_bytes(buf) as i32)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readByte", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 1];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(buf[0] as i8 as i32)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readUnsignedByte", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 1];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(buf[0] as i32)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readUnsignedShort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 2];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(u16::from_be_bytes(buf) as i32)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readBoolean", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 1];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Int(if buf[0] != 0 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 4];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Float(f32::from_be_bytes(buf))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut buf = [0u8; 8];
        raf_read_fully(ctx, fd_id, &mut buf)?;
        Ok(Some(Value::Double(f64::from_be_bytes(buf))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "readUTF", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let mut len_buf = [0u8; 2];
        raf_read_fully(ctx, fd_id, &mut len_buf)?;
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut str_buf = vec![0u8; len];
        raf_read_fully(ctx, fd_id, &mut str_buf)?;
        // JO3-3 (round 15 wave 4, lane jdkonly5): `native-io`'s units-exact
        // codec -- the JDK's decode (an unpaired surrogate is returned, not
        // refused), its messages, and a fresh result `String`.
        if cratonvm_native_io::data_utf_units_exact() {
            return cratonvm_native_io::read_utf_payload_units(ctx, &str_buf);
        }
        // MODIFIED UTF-8, not UTF-8. `DataInput.readUTF` reads "a string that
        // has been encoded using a modified UTF-8 format", and it specifies
        // `UTFDataFormatException` for "bytes [that] do not represent a valid
        // modified UTF-8 encoding of a string". `String::from_utf8_lossy`
        // answered `U+FFFD` for exactly those bytes — a silent substitution
        // where the spec mandates a refusal — and got `U+0000` (`C0 80` in
        // modified UTF-8) and every supplementary character (a SIX-byte
        // surrogate pair, not one four-byte sequence) wrong even on valid input.
        // `decode_modified_utf8` is `native-io`'s codec, the one
        // `DataOutputStream.writeUTF` already uses, so the tree now has ONE
        // spelling of this wire format instead of one per crate.
        let s = cratonvm_native_io::decode_modified_utf8(&str_buf)
            .map_err(|e| raf_utf_data_format(ctx, &e))?;
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Bridge);
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
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- DataOutput interface methods ---
    // Every `DataOutput` method below is `void` and declares "@throws
    // IOException if an I/O error occurs", so the exception is the only channel
    // it has to report a failed write. `let _ = ctx.fd_table().rw_write(..)`
    // therefore made the whole family incapable of saying "no": a record-append
    // loop against a full volume, a read-only reopen or a stale descriptor ran
    // to completion, and the caller's next act was to record the rows as
    // durable. The sibling `write([B)V` a hundred lines up already propagated,
    // which is the tell that this was accident rather than policy.
    r.register(raf, "writeInt", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.fd_table()
            .rw_write(fd_id as u32, &v.to_be_bytes())
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeLong", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = match args.get(1) {
            Some(Value::Long(l)) => *l,
            _ => 0,
        };
        ctx.fd_table()
            .rw_write(fd_id as u32, &v.to_be_bytes())
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeShort", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        ctx.fd_table()
            .rw_write(fd_id as u32, &v.to_be_bytes())
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeChar", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u16;
        ctx.fd_table()
            .rw_write(fd_id as u32, &v.to_be_bytes())
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeByte", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        ctx.fd_table()
            .rw_write(fd_id as u32, &[v])
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeBoolean", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.fd_table()
            .rw_write(fd_id as u32, &[if v != 0 { 1 } else { 0 }])
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeFloat", "(F)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = match args.get(1) {
            Some(Value::Float(f)) => *f,
            _ => 0.0,
        };
        // `writeInt(Float.floatToIntBits(v))`: the canonical NaN (round 15
        // wave 5, lane jdkonly7).
        let bits = crate::lang_math::java_float_to_int_bits(v);
        ctx.fd_table()
            .rw_write(fd_id as u32, &bits.to_be_bytes())
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeDouble", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let v = match args.get(1) {
            Some(Value::Double(d)) => *d,
            _ => 0.0,
        };
        // `writeLong(Double.doubleToLongBits(v))`: the canonical NaN.
        let bits = crate::lang_math::java_double_to_long_bits(v);
        ctx.fd_table()
            .rw_write(fd_id as u32, &bits.to_be_bytes())
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeUTF", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        // JO3-3 (round 15 wave 4, lane jdkonly5): the real body is
        // `DataOutputStream.writeUTF(str, this)` -- `writeUTF(null)` is an
        // NPE (it wrote an empty record), each UTF-16 unit is encoded on its
        // own (an unpaired surrogate survives), and the over-long refusal
        // carries `tooLongMsg`'s wording. `native-io`'s codec.
        if cratonvm_native_io::data_utf_units_exact() {
            let str_obj = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                // Round 15 wave 5 (lane jdkonly7): HotSpot's helpful text.
                _ => return Err(cratonvm_native_io::write_utf_null_npe()),
            };
            let bytes = cratonvm_native_io::write_utf_payload_units(ctx, str_obj)?;
            let len = bytes.len() as u16;
            ctx.fd_table()
                .rw_write(fd_id as u32, &len.to_be_bytes())
                .map_err(|e| RuntimeError::IOException {
                    message: e.to_string(),
                })?;
            ctx.fd_table().rw_write(fd_id as u32, &bytes).map_err(|e| {
                RuntimeError::IOException {
                    message: e.to_string(),
                }
            })?;
            return Ok(None);
        }
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        // MODIFIED UTF-8, the wire format `DataOutput.writeUTF` names: `U+0000`
        // is `C0 80` (never the one-byte `00`, so the payload can carry no NUL
        // and a C reader cannot truncate it), and a supplementary character is a
        // SIX-byte surrogate pair rather than one four-byte sequence.
        // `s.as_bytes()` was plain UTF-8, which gets BOTH the length prefix and
        // the payload wrong on those two inputs — and `readUTF` cannot tell,
        // because the prefix it reads is the one this wrote. Same codec as
        // `native-io`'s `native_raf_write_utf` and as `DataOutputStream.writeUTF`.
        let bytes = cratonvm_native_io::encode_modified_utf8(&s);
        // `.min(65535)` was the same defect as the `let _ =`s above wearing a
        // different hat: the length prefix is a `u16`, so an over-long string
        // was silently TRUNCATED — and cut at a byte index, so the tail could
        // land mid-sequence — and `writeUTF` returned normally. The spec's
        // answer is a refusal, quoted on `raf_utf_data_format`. `readUTF` next
        // to this then read a short, possibly invalid record back with nothing
        // to say it had ever been complete.
        if bytes.len() > 65535 {
            return Err(raf_utf_data_format(
                ctx,
                &format!("encoded string too long: {} bytes", bytes.len()),
            ));
        }
        let len = bytes.len() as u16;
        ctx.fd_table()
            .rw_write(fd_id as u32, &len.to_be_bytes())
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        ctx.fd_table()
            .rw_write(fd_id as u32, &bytes)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeBytes", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        let _ = ctx.fd_table().rw_write(fd_id as u32, s.as_bytes());
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(raf, "writeChars", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = raf_get_fd(ctx, this).map(|v| v as i32).unwrap_or(-1);
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        // `DataOutput.writeChars` writes "two bytes … for each character",
        // i.e. one big-endian UTF-16 CODE UNIT per `char` of the Java string —
        // so a supplementary character is TWO of them, the surrogate pair.
        // `for ch in s.chars()` iterates Rust `char`s, i.e. code POINTS, and
        // `ch as u16` then truncated every supplementary character to one wrong
        // unit: the record went out one unit short with a mangled character in
        // it, and nothing downstream could tell. `encode_utf16()` is the same
        // sequence `String.charAt` walks.
        for unit in s.encode_utf16() {
            ctx.fd_table()
                .rw_write(fd_id as u32, &unit.to_be_bytes())
                .map_err(|e| RuntimeError::IOException {
                    message: e.to_string(),                })?;        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
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
        let fd = try_alloc_concurrent_synthetic(ctx, "java/io/FileDescriptor", 1)?;
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
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        raf,
        "getChannel",
        "()Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            // gc-common w20-d: the receiver was read through its address
            // from before the channel's allocation. Rooted, read back.
            let this = obj_arg(args, 0)?;
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let ctx = &mut *scope;
            let fc = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/FileChannel", 1)?;
            let this = scope.get(&this_h);
            let slot0 = scope.get_field(this, 0);
            scope.set_field(fc, 0, slot0);
            Ok(Some(Value::Object(Some(fc))))
        }, cratonvm_native_api::NativeKind::Bridge,
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
    }, cratonvm_native_api::NativeKind::Bridge);
    }

/// Read exactly `buf.len()` bytes from a RAF fd, or error.
pub fn raf_read_fully(
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
pub fn encode_file_uri_path(path: &str) -> String {
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
    // NON-ASCII IS LEFT LITERAL, and that is not a shortcut — it is what
    // `java.net.URI` does. `File.toURI()` is `new URI("file", null,
    // slashify(path), null)`, and the multi-argument constructor renders its
    // string through `URI.quote`, which escapes a character below U+0080 only
    // when the mask rejects it and otherwise **appends it unchanged**; only
    // `toASCIIString()` encodes the rest. MEASURED:
    //
    //   new File("unicode/é中文").toURI().toString()
    //     HotSpot   file:/…/unicode/é中文
    //     CratonVM  file:/…/unicode/%C3%A9%E4%B8%AD%E6%96%87
    //
    // Percent-encoding it here made `toString()` and `getPath()` disagree with
    // every real JDK, and made a URI built from a non-ASCII filename compare
    // unequal to the one HotSpot builds from the same file.
    //
    // The exception `URI.quote` itself carries is kept: a non-ASCII SPACE or
    // control character is still escaped, because those are the two classes
    // it does encode above U+0080.
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii() {
            let b = c as u8;
            if keep(b) {
                out.push(c);
            } else {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        } else if c.is_whitespace() || c.is_control() {
            let mut buf = [0u8; 4];
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        } else {
            out.push(c);
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
pub fn set_file_mtime_millis(path: &str, millis: i64) -> bool {
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
// (unixfilesystem-getbooleanattributes0-missing-native-20260804-FIXED.md).
// The `java/io/File` natives normally hide this, because `File.exists` /
// `isFile` / `isDirectory` ARE forced overrides — but only until some test
// redefines `java.io.File` (Mockito's inline mock maker does that
// process-wide for `@Mock private File f`), after which the real `File`
// bytecode runs for every instance and every one of these natives is reached.
//
// Both spellings are registered: the `0` one for JDK 22+, the bare one for
// pre-22 JDKs and for CratonVM's synthetic `java.io.FileSystem`.

/// `java.io.FileSystem.BA_EXISTS`.
pub const FS_BA_EXISTS: i32 = 0x01;
/// `java.io.FileSystem.BA_REGULAR`.
pub const FS_BA_REGULAR: i32 = 0x02;
/// `java.io.FileSystem.BA_DIRECTORY`.
pub const FS_BA_DIRECTORY: i32 = 0x04;
/// `java.io.FileSystem.BA_HIDDEN`.
pub const FS_BA_HIDDEN: i32 = 0x08;

/// `java.io.FileSystem.ACCESS_EXECUTE`.
pub const FS_ACCESS_EXECUTE: i32 = 0x01;
/// `java.io.FileSystem.ACCESS_WRITE`.
pub const FS_ACCESS_WRITE: i32 = 0x02;
/// `java.io.FileSystem.ACCESS_READ`.
pub const FS_ACCESS_READ: i32 = 0x04;

/// The bitmask the raw `getBooleanAttributes0` JNI native returns.
///
/// Note the platform split on `BA_HIDDEN`, which is NOT cosmetic:
/// `UnixFileSystem.getBooleanAttributes0` never sets it (the public wrapper
/// ORs in `isHidden(f)`, a leading-`.` check on the *name*), while
/// `WinNTFileSystem.getBooleanAttributes0` does, straight out of
/// `FILE_ATTRIBUTE_HIDDEN`, and its wrapper adds nothing. Getting this
/// backwards makes `File.isHidden()` wrong on one platform or the other.
pub fn fs_boolean_attributes0(path: &str) -> i32 {
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
pub fn fs_boolean_attributes(path: &str) -> i32 {
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
pub fn fs_check_access(path: &str, access: i32) -> bool {
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
pub fn fs_set_permission(path: &str, access: i32, enable: bool, owner_only: bool) -> bool {
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

