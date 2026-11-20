#[allow(unused_imports)]
use super::*;

/// The platform line separator, as `System.lineSeparator()` reports it.
///
/// One function rather than the open-coded copies this module and its
/// neighbours grew: `Files.write(Path, Iterable)`, `BufferedWriter.newLine()`
/// and `Properties.store` must all terminate a line the same way, and the way
/// they came to disagree is that each read the property — or failed to — on its
/// own. The `line.separator` property is the authority, because a
/// `-Dline.separator=` override has to be honoured and a bare `cfg!(windows)`
/// cannot see one; the compile-time platform default is the fallback for a VM
/// so early in bootstrap that no property map exists yet, and it must be
/// `"\r\n"` on Windows rather than the `"\n"` three of these fallbacks carried.
pub fn p57_line_separator(ctx: &dyn NativeContext) -> String {
    // A `-Dline.separator=` override is still honoured: it is in the property
    // map before `initPhase1` captures the static from it. What is no longer
    // honoured is a LATER `System.setProperty`, which HotSpot also ignores --
    // see `NativeContext::captured_line_separator`.
    ctx.captured_line_separator()
}

/// The path String out of a Path's own slot — **the one read**, shared by
/// [`p57_read_path`]'s fast path and [`extract_path_string`].
///
/// Two arms, and neither can mis-fire: `read_string` REFUSES a non-String, so
/// asking the historical slot 0 of a NEW-layout Path yields `None` (it holds
/// the `fs` object) rather than a mis-decoded path, and asking the mapped slot
/// of a LEGACY-layout object — a mock, or one built before
/// `sun.nio.fs.UnixPath` was loadable — is an out-of-range read the accessor
/// answers `None` for. That is what lets one function serve both layouts
/// without a mode flag.
///
/// **No Java re-entry.** Both arms are plain field reads, which is the property
/// `extract_path_string`'s callers relied on before this existed;
/// `p57_read_path`'s `toString()` fallback stays where it was, above only the
/// callers that already accepted it.
pub fn p57_path_slot_string(ctx: &dyn NativeContext, path_obj: ObjectRef) -> Option<String> {
    p57_path_slot_string_as(ctx, path_obj, true)
}

/// The slot's text exactly as STORED (the JDK's own `\` form on Windows) — what
/// a real `WindowsPath.toString()` returns, with no display-time rewriting.
pub fn p57_path_slot_string_stored(
    ctx: &dyn NativeContext,
    path_obj: ObjectRef,
) -> Option<String> {
    p57_path_slot_string_as(ctx, path_obj, false)
}

pub fn p57_path_slot_string_as(
    ctx: &dyn NativeContext,
    path_obj: ObjectRef,
    canonical: bool,
) -> Option<String> {
    let slots = p57_path_slots();
    // The slot holds the JDK's own form (`\` on Windows); this VM's natives all
    // work on the `/`-canonical spelling — see `path_layout::stored_form`.
    let canon = |t: String| {
        if canonical {
            cratonvm_native_api::path_layout::canonical_form(&t).into_owned()
        } else {
            t
        }
    };
    if let Value::Object(Some(s)) = ctx.get_field(path_obj, slots.string) {
        if let Some(t) = ctx.read_string(s) {
            return Some(canon(t));
        }
    }
    if slots.string != 0 {
        if let Value::Object(Some(s)) = ctx.get_field(path_obj, 0) {
            if let Some(t) = ctx.read_string(s) {
                return Some(canon(t));
            }
        }
    }
    // Concrete UnixPath bytecode in either runtime policy (`resolve`,
    // `getParent`, `normalize`, `FileSystem.getPath`, ...) leaves `stringValue`
    // null until its own `toString()` runs; the text is in `path`, the encoded
    // bytes. Decode them exactly as `UnixPath.toString()` does on Linux
    // (`Util.toString` over `sun.jnu.encoding`, UTF-8; `normalizeJavaPath` is
    // the identity). This also keeps native provider bridges correct in
    // compatible mode when they receive a Path built by real bytecode.
    // Unlike the `toString()` fallback in `p57_read_path`, this allocates
    // nothing and re-enters no Java, so callers holding other references across
    // it (`extract_path_string`'s) stay safe. Synthetic Paths write both slots,
    // and take the String fast path above.
    // A Path constructed by Java need not have gone through a native
    // allocator that resolves the shared slot map. Read its loaded field by
    // name in that case, without class initialization, allocation or Java
    // re-entry. A foreign/decorating Path keeps its existing toString path.
    if p57_is_path_carrier(ctx, path_obj) {
        if let Value::Object(Some(path)) = ctx.get_field_by_name(path_obj, "path") {
            if let Some(t) = ctx.read_string(path) {
                return Some(canon(t));
            }
            if let Some((cratonvm_types::ArrayElementType::Byte, len)) = ctx.array_shape(path) {
                let mut buf = vec![0u8; len];
                let n = ctx.read_byte_array_into(path, 0, &mut buf);
                buf.truncate(n);
                return Some(canon(String::from_utf8_lossy(&buf).into_owned()));
            }
        }
    }
    if let Some(bytes_slot) = slots.bytes {
        if let Value::Object(Some(arr)) = ctx.get_field(path_obj, bytes_slot) {
            let len = ctx.array_length(arr);
            let mut buf = vec![0u8; len];
            let n = ctx.read_byte_array_into(arr, 0, &mut buf);
            buf.truncate(n);
            return Some(canon(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
    None
}

/// The text of `path_obj`, with `path_obj` handed back CURRENT.
///
/// gc-common w21-b
/// (`common-w20d-p57-read-path-java-fallback-leaves-callers-refs-stale-FIXED-20260923`):
/// a Path whose own slot holds no String -- a foreign or
/// decorating Path (Quarkus's `PathWrapper`, `ZipPath`, an application's own)
/// -- is answered by its `toString()`, which is Java and allocates, so a
/// moving collection can run inside this call. The Path is therefore taken by
/// `&mut` (the `NativeContext::pin_native_root` funnel convention): it is
/// pinned across the fallback and written back, and every caller that kept
/// using it afterwards is current by construction. The slot-backed Paths this
/// VM mints never reach the fallback and pay no pin.
///
/// Anything ELSE the caller holds across this call -- a second Path, an
/// `ObjectRef` read out of `args`, `args` itself -- is NOT protected by it:
/// use [`p57_read_path_pair`] for two Paths and [`p57_read_path_arg`] when
/// the native reads further arguments afterwards.
pub fn p57_read_path(ctx: &mut dyn NativeContext, path_obj: &mut ObjectRef) -> String {
    if let Some(raw) = p57_path_slot_string(ctx, *path_obj) {
        return p57_to_os_path(&raw);
    }
    let pin = ctx.pin_native_root(*path_obj);
    let text = p57_read_foreign_path(ctx, *path_obj);
    *path_obj = ctx.read_native_pin(pin, *path_obj);
    ctx.unpin_native_roots(pin);
    text
}

/// Two Paths' text, both handed back current: each is rooted across the
/// other's read (either may be a foreign Path whose `toString()` collects).
/// No pin at all when both are slot-backed.
pub fn p57_read_path_pair(
    ctx: &mut dyn NativeContext,
    a: &mut ObjectRef,
    b: &mut ObjectRef,
) -> (String, String) {
    if let (Some(x), Some(y)) = (p57_path_slot_string(ctx, *a), p57_path_slot_string(ctx, *b)) {
        return (p57_to_os_path(&x), p57_to_os_path(&y));
    }
    let base = ctx.pin_native_root(*a);
    let b_pin = ctx.pin_native_root(*b);
    let ta = p57_read_path(ctx, a);
    *b = ctx.read_native_pin(b_pin, *b);
    let tb = p57_read_path(ctx, b);
    *a = ctx.read_native_pin(base, *a);
    ctx.unpin_native_roots(base);
    (ta, tb)
}

/// The text of the Path in `args[idx]` (`obj_arg`'s null refusal first), and
/// `args` handed back CURRENT.
///
/// A native's `args` slice is a snapshot that no collection rewrites
/// (`common-w11c-rb-get-bundle-args-bare-across-java-FIXED-20260923`), so a
/// native that reads a further argument -- or the Path again -- after the
/// read of a foreign Path read a pre-collection address. The fast path
/// borrows `args` unchanged; the fallback roots every object argument across
/// the read and answers a refreshed copy. The one shape for such callers:
///
/// ```ignore
/// let (p, args) = p57_read_path_arg(ctx, args, 0)?;
/// let args = &*args;
/// ```
pub fn p57_read_path_arg<'a>(
    ctx: &mut dyn NativeContext,
    args: &'a [Value],
    idx: usize,
) -> Result<(String, std::borrow::Cow<'a, [Value]>), cratonvm_types::error::MethodCallFailed> {
    let path_obj = obj_arg(args, idx)?;
    if let Some(raw) = p57_path_slot_string(ctx, path_obj) {
        return Ok((p57_to_os_path(&raw), std::borrow::Cow::Borrowed(args)));
    }
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let handles: Vec<Option<cratonvm_native_api::NativeHandle>> = args
        .iter()
        .map(|v| match v {
            Value::Object(Some(o)) => Some(scope.root(*o)),
            _ => None,
        })
        .collect();
    let mut path_now = path_obj;
    let text = p57_read_path(&mut *scope, &mut path_now);
    let current: Vec<Value> = args
        .iter()
        .zip(&handles)
        .map(|(v, h)| match h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => *v,
        })
        .collect();
    Ok((text, std::borrow::Cow::Owned(current)))
}

/// [`p57_read_path`]'s Java half, for a Path whose slot holds no String. Runs
/// `toString()` (Compatible) or the receiver's own `toString()` bytecode
/// (`--real-jdk`); the caller pins `path_obj` across it.
pub fn p57_read_foreign_path(ctx: &mut dyn NativeContext, path_obj: ObjectRef) -> String {
    // Fast path (in `p57_read_path`): CratonVM's own synthetic Path layout
    // stores the path String directly in its `string` slot (see
    // `P57PathSlots`).
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
    //
    // STACK-OVERFLOW GUARD (found 2026-07-19 investigating a real crash: JIT
    // method-stats work on an unrelated ES/Lucene test hit
    // EXCEPTION_STACK_OVERFLOW at gen_heap::get_field, root-caused via the
    // dispatch_trace ring to `TestRuleTemporaryFilesCleanup.initializeJavaTempDir`
    // -> native Path.toString() repeating 256/256 times with zero variation).
    //
    // This fallback's `invoke_virtual(path_obj, "toString", ...)` was written
    // assuming dispatch lands somewhere OTHER than back here — either the
    // (separate) dead-dispatch-to-Object.toString() bug fixed the same day in
    // `path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`,
    // or a genuine delegating wrapper's own real bytecode `toString()`. A
    // THIRD same-day fix
    // (`path-tostring-indy-stringconcat-dead-dispatch-FIXED.md`,
    // `vm_exec.rs`'s `invoke_on_class_shared_inner`) made dispatch correctly
    // receiver-aware: ANY Path-subtype receiver's `toString()` now routes
    // straight back to this exact native (`p57_path_display_string` ->
    // `p57_read_path`). For a `path_obj` whose field-0 fast-path read keeps
    // failing (nothing about the object changes between calls), that
    // redirect recurses into this same function forever — a real, silent
    // EXCEPTION_STACK_OVERFLOW, not a Java StackOverflowError (native
    // recursion via `ctx.invoke_virtual` is invisible to every one of the
    // interpreter's counted recursion guards; see
    // `ES-CRASH-20260719-lucene-jit-getfield-stack-overflow-FIXED.md`).
    //
    // A thread-local re-entrancy flag breaks the cycle: the first call takes
    // the real dispatch as before (the common, legitimate delegating-wrapper
    // case terminates immediately since IT dispatches into different, real
    // bytecode); a NESTED re-entry into this exact fallback — which can only
    // happen via the recursive-redirect case above — returns the same benign
    // empty-string fallback the final `_ =>` arm already uses for other
    // failures, instead of recursing again.
    //
    // Under jdk-only the receiver's own `toString()` BYTECODE runs instead
    // (`p57_foreign_path_to_string`), which can never re-enter this native
    // for the same receiver, so a nested fallback for a different receiver
    // (a wrapper whose delegate is itself a real Path) is not starved.
    if ctx.is_jdk_only() {
        return p57_foreign_path_to_string(ctx, path_obj)
            .map(|raw| p57_to_os_path(&raw))
            .unwrap_or_default();
    }
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
            p57_to_os_path(&raw)        }
        _ => String::new(),
    };
    IN_TOSTRING_FALLBACK.with(|f| f.set(false));
    result
}

/// jdk-only: a real (non-carrier) Path's own `toString()` bytecode result.
/// `invoke_virtual_bytecode_only` resolves the receiver class's declared
/// `toString` and runs it without consulting the native registry, so the
/// force-native `Path.toString` entry cannot bounce it back here. The
/// per-thread receiver stack only guards a real Path whose bytecode
/// `toString` genuinely calls back into `Path.toString` on itself. `None`
/// when nothing below `Object` declares `toString` -- `Object.toString()`'s
/// `Name@hash` is no path text.
pub fn p57_foreign_path_to_string(ctx: &mut dyn NativeContext, path_obj: ObjectRef) -> Option<String> {
    let mut cid = Some(ctx.class_id_of_object(path_obj));
    let mut declared = false;
    while let Some(c) = cid {
        if ctx.class_name_arc_of_id(c).as_deref() == Some("java/lang/Object") {
            break;
        }
        if ctx.class_declares_method(c, "toString", "()Ljava/lang/String;") {
            declared = true;
            break;
        }
        cid = ctx.superclass_of(c);
    }
    if !declared {
        return None;
    }
    // gc-common w15-f: each entry is the receiver's native PIN (and its
    // address when pushed, the pin's fallback), compared through the pin's
    // current address. The stack held bare addresses across the `toString`
    // upcall, so a collection inside it made the guard miss the moved
    // receiver's re-entry (unbounded recursion) and fire for whatever object
    // was later allocated at the vacated address (an empty path).
    thread_local! {
        static ACTIVE: std::cell::RefCell<Vec<(usize, ObjectRef)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let reentered = ACTIVE.with(|a| {
        a.borrow()
            .iter()
            .any(|&(pin, pushed)| ctx.read_native_pin(pin, pushed) == path_obj)
    });
    if reentered {
        return None;
    }
    let pin = ctx.pin_native_root(path_obj);
    ACTIVE.with(|a| a.borrow_mut().push((pin, path_obj)));
    let r = ctx.invoke_virtual_bytecode_only(path_obj, "toString", "()Ljava/lang/String;", &[]);
    ACTIVE.with(|a| {
        a.borrow_mut().pop();
    });
    ctx.unpin_native_roots(pin);
    match r {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// `Path.toFile()` of the default file system: the JDK body is
/// `new File(toString())`, so the answer is whatever `File(String)` makes of
/// the text, normalized by the platform `FileSystem` (on Windows `C:/a//b/`
/// is `C:\a\b`). Over a real `java.io.File` the VM runs that constructor.
///
/// The slot text of a VM Path keeps `/` on Windows, and a `File` written
/// straight from it answered `getPath()` with `/`, `equals` / `hashCode`
/// unequal to the same `new File(...)`, and a child `new File(dir, "x.jar")`
/// that was not absolute: a `URLClassLoader` over
/// `Files.createTempDirectory(..).toFile()` found nothing under
/// `--compatible` (`--real-jdk` runs the interface body). Interpreter round
/// i2 wave 57, lane L5; probe `L5W57CompatPathToFile`.
///
/// A mounted jar/jrt entry's sentinel text and a synthetic `File` (no real
/// JDK) keep the direct write, whose WIDTH and two extra writes are
/// `cratonvm_native_api::file_layout`'s (a one-slot File keeps
/// `prefixLength = 0`, which made real `File` bytecode call every path
/// relative).
pub fn p57_path_to_file(
    ctx: &mut dyn NativeContext,
    p: &str,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    if vfs_decode(p).is_none()
        && !ctx.is_class_synthetic_stub("java/io/File")
        && ctx.method_exists("java/io/File", "<init>", "(Ljava/lang/String;)V")
    {
        if let Some(Value::Object(Some(file))) = ctx.new_object("java/io/File")? {
            let file_pin = ctx.pin_native_root(file);
            let s = ctx.create_string(p);
            let file = ctx.read_native_pin(file_pin, file);
            let ran = ctx.invoke_special(
                "java/io/File",
                "<init>",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(file)), Value::Object(Some(s))],
            );
            let file = ctx.read_native_pin(file_pin, file);
            ctx.unpin_native_roots(file_pin);
            ran?;
            return Ok(Some(Value::Object(Some(file))));
        }
    }
    let w = cratonvm_native_api::file_layout::alloc_width(ctx);
    let file = try_alloc_concurrent_synthetic(ctx, "java/io/File", w)?;
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh File (native stale-local family).
    let file_pin = ctx.pin_native_root(file);
    let s = ctx.create_string(p);
    let file = ctx.read_native_pin(file_pin, file);
    ctx.set_field(file, 0, Value::Object(Some(s)));
    cratonvm_native_api::file_layout::write(ctx, file, s, p);
    ctx.unpin_native_roots(file_pin);
    Ok(Some(Value::Object(Some(file))))
}

/// Convert a Java-style path to an OS-native path.
/// Strips leading `/` from Windows drive paths like `/C:/foo` → `C:/foo`.
pub fn p57_to_os_path(p: &str) -> String {
    // Handle `/C:/...` — common from URI paths on Windows
    if cfg!(windows) && p.len() >= 3 && p.starts_with('/') && p.as_bytes().get(2) == Some(&b':') {
        return p[1..].to_string();
    }
    p.to_string()
}

pub fn p57_absolute_path_string(path: &str) -> String {
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
pub fn p57_trim_file_trailing_separator(path: &str) -> String {
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
        // A drive root keeps the platform's separator: HotSpot answers
        // `C:\` for `new File("C:\\").getAbsolutePath()` (interpreter round
        // i1 wave 52; `C:/` until then).
        return if cfg!(windows) {
            format!("{trimmed}\\")
        } else {
            format!("{trimmed}/")
        };
    }
    trimmed.to_string()
}

/// Match `WindowsPath` construction: an ordinary trailing separator is not a
/// name element, while every filesystem root keeps its separator. This is
/// separate from the `java.io.File` helper because virtual filesystem paths
/// carry opaque encoded roots that must never be rewritten.
#[cfg(windows)]
pub fn p57_trim_path_trailing_separator(path: &str) -> String {
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
/// `resourcestests-trailing-slash-path-normalization-FIXED-20260804.md`.
///
/// Virtual (jar/jrt) filesystem paths are excluded, exactly as in the Windows
/// twin: their sentinel-encoded string carries an entry whose trailing `/` is
/// part of the virtual-entry representation, not a redundant separator.
#[cfg(not(windows))]
pub fn p57_trim_path_trailing_separator(path: &str) -> String {
    if vfs_decode(path).is_some() {
        return path.to_string();
    }
    file_normalise_path(path)
}

#[cfg(windows)]
pub fn p57_windows_absolute_path_string(path: &str) -> String {
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
pub fn p57_parse_win_root(s: &str) -> (Option<String>, Vec<String>) {
    let (root, remainder) = p57_parse_win_root_raw(s);
    let names = remainder
        .split(|c| c == '\\' || c == '/')
        .filter(|seg| !seg.is_empty())
        .map(|seg| seg.to_string())
        .collect();
    (root, names)
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
pub fn p57_parse_root(s: &str) -> (Option<String>, Vec<String>) {
    p57_parse_win_root(s)
}

#[cfg(not(windows))]
pub fn p57_parse_root(s: &str) -> (Option<String>, Vec<String>) {
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
/// `Path.startsWith`, which is a NAME-ELEMENT rule and not a string one.
///
/// MEASURED, both spellings wrong in the same way:
///
/// ```text
///   Paths.get("a/bc").startsWith("a/b")   HotSpot false   CratonVM true
///   Paths.get("a/b/c").startsWith("")     HotSpot false   CratonVM true
/// ```
///
/// `String::starts_with` is a prefix test on TEXT, and a path is not text: it
/// is a root plus a sequence of names. The first row is the dangerous one —
/// `startsWith` is how a program asks "is this file inside that directory",
/// which is the shape of nearly every path-traversal check ever written. A
/// text prefix answers yes for `/appsecret` under `/app`.
///
/// The rule, from `sun.nio.fs.UnixPath`: the roots must be the same, `other`
/// may not have more name elements, and every one of its names must equal the
/// name at the same index. The EMPTY path has no names and no root, and is a
/// prefix of nothing but itself.
pub fn p57_starts_with(this: &str, other: &str) -> bool {
    let (r1, n1) = p57_parse_root(this);
    let (r2, n2) = p57_parse_root(other);
    if r1 != r2 {
        return false;
    }
    if r2.is_none() && n2.is_empty() {
        return r1.is_none() && n1.is_empty();
    }
    if n2.len() > n1.len() {
        return false;
    }
    n1[..n2.len()] == n2[..]
}

/// `Path.endsWith`, the mirror rule — with the asymmetry the JDK actually has.
///
/// ```text
///   Paths.get("ab/c").endsWith("b/c")     HotSpot false   CratonVM true
///   Paths.get("/a/b").endsWith("a/b")     HotSpot true  (a RELATIVE suffix
///                                                        matches an absolute
///                                                        path's tail)
/// ```
///
/// `startsWith` is symmetric in the root — both operands must have the same
/// one. `endsWith` is not: a relative `other` matches the tail of an absolute
/// `this`, while an ABSOLUTE `other` must match the whole path including its
/// root. That asymmetry is the piece a paraphrase drops.
pub fn p57_ends_with(this: &str, other: &str) -> bool {
    let (r1, n1) = p57_parse_root(this);
    let (r2, n2) = p57_parse_root(other);
    if r2.is_none() && n2.is_empty() {
        return r1.is_none() && n1.is_empty();
    }
    if n2.len() > n1.len() {
        return false;
    }
    if r2.is_some() && (r1 != r2 || n1.len() != n2.len()) {
        return false;
    }
    n1[n1.len() - n2.len()..] == n2[..]
}

/// Is `other` a `Path` at all?
///
/// `Path.equals(Object)` read the argument's path unconditionally, so
/// `Paths.get("a/b/c").equals("a/b/c")` — a `String` — answered **true**.
/// MEASURED against HotSpot, which answers false. An `equals` that admits a
/// foreign type breaks the contract in both directions at once: it is not
/// symmetric (`"a/b/c".equals(path)` is false), so a `HashSet` or a `List
/// .contains` gives different answers depending on which side it holds.
///
/// The test is deliberately generous, because a `false` here would be a
/// REGRESSION for any real JDK `Path` implementation that reaches this native:
/// this VM's own representation is the synthetic `java/nio/file/Path`, and the
/// JDK's are all named `*Path` (`sun.nio.fs.UnixPath`, `WindowsPath`,
/// `jdk.nio.zipfs.ZipPath`). Anything else — a `String` above all — is not one.
pub fn p57_is_path_object(ctx: &mut dyn NativeContext, other: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(other);
    match ctx.class_name_arc_of_id(cid).as_deref() {
        Some("java/nio/file/Path") => true,
        Some(name) => {
            name.rsplit('/').next().is_some_and(|s| s.ends_with("Path"))
                || ctx.synthetic_implements_declared(cid, "java/nio/file/Path")
        }
        None => false,
    }
}

pub fn p57_name_elements(path: &str) -> Vec<String> {
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
pub fn p57_posix_parent_of(path: &str) -> String {
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
pub fn p57_win_is_absolute(s: &str) -> bool {
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
pub fn p57_win_parent_of(path: &str) -> String {
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


/// True if `path_obj`'s recorded owning FileSystem (the `fs` slot) is a
/// virtual (mounted-jar or runtime-image) FileSystem. Used by `Path.toString`
/// to render '/' for relative paths that belong to such a FS.
pub fn path_owned_by_virtual_fs(ctx: &mut dyn NativeContext, path_obj: ObjectRef) -> bool {
    if let Value::Object(Some(fs)) = ctx.get_field(path_obj, p57_path_slots_for(ctx.vm_identity()).fs) {
        p57_fs_is_virtual(ctx, fs)
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
pub fn p57_validate_windows_path(s: &str) -> Result<(), &'static str> {
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
    // A UNC prefix (`\\`/`//`, already past the `\\?\` long-path form above)
    // must be followed by a non-empty hostname before the next separator —
    // `WindowsPathParser.parse` throws "UNC path is missing hostname" when it
    // is not. `Paths.get("//")` is the minimal reproduction: two separators
    // and nothing after. Legal on Linux (a plain relative-looking string),
    // fatal on Windows, which is why this whole function is a no-op off that
    // platform — see the `cfg!(windows)` guard above.
    if bytes.len() >= 2
        && (bytes[0] == b'\\' || bytes[0] == b'/')
        && (bytes[1] == b'\\' || bytes[1] == b'/')
    {
        let hostname_empty = bytes.len() == 2 || bytes[2] == b'\\' || bytes[2] == b'/';
        if hostname_empty {
            return Err("UNC path is missing hostname");
        }
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

/// The root/remainder split [`p57_parse_win_root`] performs, before the
/// remainder is cut into names — factored out so
/// [`p57_windows_check_and_normalize`] can run `WindowsPathParser`'s
/// character-by-character walk over the EXACT SAME remainder
/// `p57_parse_win_root` would split, so the two can never disagree about
/// where the root ends. `root`, when present, still carries `\` separators
/// (as `p57_parse_win_root` always has) — callers that want the `/`-canonical
/// form fold it themselves, same as every existing caller here already does.
pub fn p57_parse_win_root_raw(s: &str) -> (Option<String>, String) {
    let is_sep = |c: u8| c == b'\\' || c == b'/';
    if let Some((_tag, _container, entry)) = vfs_decode(s) {
        let entry = entry.trim_start_matches('/');
        return (Some("/".to_string()), entry.to_string());
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
        let remainder = it.next().unwrap_or("").to_string();
        return (Some(format!("\\\\{}\\{}\\", server, share)), remainder);
    }
    if wb.len() >= 2 && is_sep(wb[0]) && is_sep(wb[1]) {
        let after = &work[2..];
        let mut it = after.splitn(3, |c| c == '\\' || c == '/');
        let server = it.next().unwrap_or("");
        let share = it.next().unwrap_or("");
        if !server.is_empty() && !share.is_empty() {
            let remainder = it.next().unwrap_or("").to_string();
            return (Some(format!("\\\\{}\\{}\\", server, share)), remainder);
        }
    }
    if wb.len() >= 2 && (wb[0] as char).is_ascii_alphabetic() && wb[1] == b':' {
        let drive = format!("{}:", wb[0] as char);
        if wb.len() >= 3 && is_sep(wb[2]) {
            return (Some(format!("{}\\", drive)), work[3..].to_string());
        }
        return (Some(drive), work[2..].to_string());
    }
    if !wb.is_empty() && is_sep(wb[0]) {
        return (Some("\\".to_string()), work[1..].to_string());
    }
    (None, work.to_string())
}

/// `sun.nio.fs.WindowsPathParser.reservedChars`, MEASURED against
/// `lib/src.zip` (JDK 25.0.3+9-LTS): control characters below ` `, plus
/// `<>:"|?*`. The colon here is why a stray `:` — one that is not the second
/// character of a drive prefix, already consumed into the root by
/// [`p57_parse_win_root_raw`] before this runs — refuses with the SAME
/// reason/index shape as any other reserved character, where
/// `p57_validate_windows_path` used to special-case it with a different,
/// index-less message.
#[cfg(windows)]
pub fn p57_win_reserved_unit(u: u16) -> bool {
    u < 0x20 || matches!(u, 0x3C | 0x3E | 0x3A | 0x22 | 0x7C | 0x3F | 0x2A)
}

/// Transcribed from `sun.nio.fs.WindowsPathParser.normalize` (`lib/src.zip`,
/// JDK 25.0.3+9-LTS): a single left-to-right walk over the post-root
/// remainder that collapses runs of `/`/`\` to ONE separator and refuses,
/// with the JDK's own reason text and index, wherever it does — a space
/// immediately before a separator or at the end of the path
/// (`"Trailing char"`), or a control/reserved character (`"Illegal char"`).
/// `G22-1` §5(a): MEASURED divergences this closes — `Paths.get("a//b")`
/// keeping both separators, `Paths.get("a ")` (trailing space) being
/// accepted, and the illegal-char message carrying neither the character nor
/// an index.
///
/// Indices are UTF-16 code-unit offsets into `remainder` (not `char`s), the
/// same discipline [`p57_globs_to_regex`] uses, so a message quoting an
/// ASCII/BMP name character indexes the same position `String.charAt` would.
/// Unlike the JDK this collapses runs onto `/`, not `\` — the internal form
/// this crate stores stays `/` (see `p57_alloc_path`'s own doc); the walk and
/// refusal shapes are otherwise the JDK's, transcribed statement for
/// statement. Does NOT special-case the `\\?\` long-path prefix rewrite the
/// JDK applies to its own exception `input` field — untested by any measured
/// row and left for whoever needs it.
#[cfg(windows)]
pub fn p57_windows_normalize_rest(remainder: &str) -> Result<String, (&'static str, char, usize)> {
    let units: Vec<u16> = remainder.encode_utf16().collect();
    let len = units.len();
    let next_non_slash = |units: &[u16], mut off: usize| {
        while off < len && matches!(units[off], 0x2F | 0x5C) {
            off += 1;
        }
        off
    };
    let mut out: Vec<u16> = Vec::with_capacity(len);
    let mut src_pos = next_non_slash(&units, 0);
    let mut path_pos = src_pos;
    let mut last_c: u16 = 0;
    while path_pos < len {
        let c = units[path_pos];
        if c == 0x2F || c == 0x5C {
            if last_c == 0x20 {
                return Err(("Trailing char", ' ', path_pos - 1));
            }
            out.extend_from_slice(&units[src_pos..path_pos]);
            path_pos = next_non_slash(&units, path_pos);
            if path_pos != len {
                out.push(0x2F);
            }
            src_pos = path_pos;
        } else {
            if p57_win_reserved_unit(c) {
                let ch = char::from_u32(c as u32).unwrap_or('\u{FFFD}');
                return Err(("Illegal char", ch, path_pos));
            }
            last_c = c;
            path_pos += 1;
        }
    }
    if src_pos != path_pos {
        if last_c == 0x20 {
            return Err(("Trailing char", ' ', path_pos - 1));
        }
        out.extend_from_slice(&units[src_pos..path_pos]);
    }
    Ok(String::from_utf16_lossy(&out))
}

/// `Paths.get`/`File.toPath()` input handling: validate AND collapse
/// redundant separators in one pass, matching `sun.nio.fs.WindowsPathParser`
/// (`parse` + `normalize`). Returns the collapsed path (root, if any, still
/// `\`-joined; the whole thing is folded to `/` by the caller exactly as the
/// old trim-only path already did) to hand to [`p57_alloc_path`], or
/// `(input, reason, index)` for the `InvalidPathException` 3-arg constructor.
#[cfg(windows)]
pub fn p57_windows_check_and_normalize(path: &str) -> Result<String, (String, String, i32)> {
    let (root, remainder) = p57_parse_win_root_raw(path);
    let root_unit_len: usize = root.as_deref().map_or(0, |r| r.encode_utf16().count());
    match p57_windows_normalize_rest(&remainder) {
        Ok(collapsed) => Ok(format!("{}{}", root.unwrap_or_default(), collapsed)),
        Err((reason_prefix, ch, local_index)) => Err((
            path.to_string(),
            format!("{reason_prefix} <{ch}>"),
            (root_unit_len + local_index) as i32,
        )),
    }
}

/// Build and throw the real `InvalidPathException(input, reason, index)`.
/// Shared by [`p57_alloc_path_checked`]'s two refusal shapes (the legacy
/// index-less `p57_validate_windows_path` reason, and
/// [`p57_windows_check_and_normalize`]'s indexed one) so both go through one
/// pinning discipline.
pub fn p57_throw_invalid_path(
    ctx: &mut dyn NativeContext,
    input: &str,
    reason: &str,
    index: i32,
) -> MethodCallResult {
    // Round 15 wave 5 (lane jdkonly7): through `construct_throwable` (the
    // object rooted across `<init>`, a throwing constructor is the answer)
    // with fresh strings; this was `new_object` + an `<init>` whose failure
    // was ignored (the half-built exception was thrown) and two interned
    // strings -- the family `r15w3-jdkonly3-residual-new-object-init-builders`
    // converted elsewhere. The input is rooted across the reason's allocation.
    let input_str = ctx.create_string_uninterned(input);
    let input_pin = ctx.pin_native_root(input_str);
    let reason_str = ctx.create_string_uninterned(reason);
    let input_str = ctx.read_native_pin(input_pin, input_str);
    ctx.unpin_native_roots(input_pin);
    match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/nio/file/InvalidPathException",
        "(Ljava/lang/String;Ljava/lang/String;I)V",
        &[
            Value::Object(Some(input_str)),
            Value::Object(Some(reason_str)),
            Value::Int(index),
        ],
    ) {
        Ok(Some(exc)) => Err(MethodCallFailed::ExceptionThrown(exc)),
        Err(thrown) => Err(thrown),
        Ok(None) => Err(RuntimeError::IllegalArgumentException {
            message: if index >= 0 {
                format!("{reason} at index {index}: {input}")
            } else {
                format!("{reason}: {input}")
            },
        }
        .into()),
    }
}

/// The `InvalidPathException` `(input, reason, index)` the platform's path
/// parser throws for `path`, or `None` to accept it (round 15 wave 5, lane
/// jdkonly7). For the `--compatible` `Path.of` / default `FileSystem.getPath`
/// natives, which built ANY string into a `Path` (w4a: `Path.of("a\u0000b")`
/// under `--compatible` returned a Path; HotSpot throws). The refusals
/// [`p57_alloc_path_checked`] makes (`Paths.get`), asked without its
/// separator collapsing so an accepted path is stored exactly as before, plus
/// `UnixPath.checkNotNul`'s `"Nul character not allowed"` off Windows, which
/// no arm asked. `CRATONVM_COMPAT_PATH_OF_CHECKED=0` accepts everything again
/// (read only for a path that would be refused).
pub fn p57_path_of_refusal(path: &str) -> Option<(String, String, i32)> {
    if jarfs_decode(path).is_some() || vfs_decode(path).is_some() {
        return None;
    }
    let refusal = p57_path_parse_refusal(path)?;
    true
        .then_some(refusal)
}

/// `WindowsPathParser.parse(input, true)`'s refusal (the one shared port,
/// `native-io`'s `windows_path_parser_refusal`, which `native-io`'s own
/// `Paths.get` doors already throw), under
/// `CRATONVM_COMPAT_WINDOWS_PATH_PARSER` (default on, read only on a refusal;
/// `=0` keeps this crate's legacy checks alone, as before round 15 wave 7).
#[cfg(windows)]
pub fn p57_windows_parser_refusal(path: &str) -> Option<cratonvm_native_io::WindowsPathRefusal> {
    let refusal = cratonvm_native_io::windows_path_parser_refusal(path)?;
    true
        .then_some(refusal)
}

pub fn p57_path_parse_refusal(path: &str) -> Option<(String, String, i32)> {
    #[cfg(windows)]
    {
        if let Some(r) = p57_windows_parser_refusal(path) {
            return Some((r.input, r.reason, r.index));
        }
        if let Err(refusal) = p57_windows_check_and_normalize(path) {
            return Some(refusal);
        }
    }
    if let Err(reason) = p57_validate_windows_path(path) {
        return Some((path.to_string(), reason.to_string(), -1));
    }
    if !cfg!(windows) && path.contains('\0') {
        return Some((
            path.to_string(),
            "Nul character not allowed".to_string(),
            -1,
        ));
    }
    None
}

/// Validate `path` before wrapping it in a synthetic p57 `Path` object;
/// throws a real `java.nio.file.InvalidPathException` (matching real JDK)
/// instead of silently accepting any string. See [`p57_validate_windows_path`]
/// and, for the separator-collapsing half, [`p57_windows_check_and_normalize`].
pub fn p57_alloc_path_checked(ctx: &mut dyn NativeContext, path: &str) -> MethodCallResult {
    // Round 15 wave 7 (lane drainA; R4 of
    // `r15w4-jdkonly4-builder-review-findings`): the JDK parser's indexed
    // refusal first, so the index-less legacy check below never answers a
    // reserved character, a UNC host/share or a `\\?\` refusal.
    #[cfg(windows)]
    {
        if jarfs_decode(path).is_none() && vfs_decode(path).is_none() {
            if let Some(r) = p57_windows_parser_refusal(path) {
                return p57_throw_invalid_path(ctx, &r.input, &r.reason, r.index);
            }
        }
    }
    if let Err(reason) = p57_validate_windows_path(path) {
        return p57_throw_invalid_path(ctx, path, reason, -1);
    }
    // Round 15 wave 5 (lane jdkonly7): off Windows the only refusal is
    // `UnixPath`'s NUL check, which nothing asked.
    if !cfg!(windows) {
        if let Some((input, reason, index)) = p57_path_of_refusal(path) {
            return p57_throw_invalid_path(ctx, &input, &reason, index);
        }
    }
    #[cfg(windows)]
    {
        // Sentinel-encoded internal strings (jar/jrt entries) are never real
        // Windows path syntax — same exclusion `p57_trim_path_trailing_separator`
        // and `p57_alloc_path` already make for them.
        if jarfs_decode(path).is_none() && vfs_decode(path).is_none() {
            return match p57_windows_check_and_normalize(path) {
                Ok(normalized) => Ok(Some(Value::Object(Some(p57_alloc_path(ctx, &normalized)?)))),
                Err((input, reason, index)) => p57_throw_invalid_path(ctx, &input, &reason, index),
            };
        }
    }
    Ok(Some(Value::Object(Some(p57_alloc_path(ctx, path)?))))
}

pub fn p57_alloc_path(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // THE PLATFORM CLASS'S OWN IDENTITY, not only its layout. Until
    // 2026-09-16 this allocated under the name `java/nio/file/Path` --
    // abstract in every real image -- and `path_layout` separately wrote the
    // real class's fields BY INDEX so bytecode dispatched to real
    // `UnixPath`/`WindowsPath` methods would read the right slots. That fixed
    // every VALUE, but `Object.getClass()`/`instanceof` still answered the
    // interface, which is what blocks `sun.nio.fs.*FileSystemProvider`'s own
    // `toUnixPath`/`toWindowsPath` (`instanceof UnixPath` -- see
    // `retired_shadow.rs`'s `the_provider_family_is_retired_by_no_table`).
    // `alloc_concrete` mints the SAME class `path_layout::impl_class()`
    // resolves -- one name, read by both -- so the two cannot drift the way
    // `native-io/src/concrete_receiver.rs`'s module doc warns a hand-kept
    // second copy would. `nfields = 0`: every slot this object needs is
    // already a declared field on the concrete class (`fs`, `path`,
    // `stringValue` on Unix; `path` alone on Windows); there is no appended
    // private state the way `sun.nio.ch`'s minted receivers carry.
    // `alloc_platform_path_object` -- the concrete class when it exists; when
    // it does not (a synthetic-JDK build), an object still wide enough for the
    // path layout. `nfields = 0` alone gave that case zero slots.
    let obj = alloc_platform_path_object(ctx);
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
    p57_write_path_fields(ctx, obj, &stored)
}


/// The `OpenOption`s the nio open paths act on, as scanned by
/// [`fsp_scan_open_options`].
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct P57OpenFlags {
    /// `StandardOpenOption.APPEND` — open at end-of-file instead of truncating.
    pub append: bool,
    /// `StandardOpenOption.CREATE_NEW` — fail if the file already exists.
    pub create_new: bool,
    /// `LinkOption.NOFOLLOW_LINKS`. It implements `OpenOption` as well as
    /// `CopyOption`, so it is legal in every `open`/`newXStream` varargs list —
    /// see [`p57_nofollow_reject`] for what it obliges us to do.
    pub nofollow: bool,
    /// `StandardOpenOption.READ`. Harmless on the input side (it is what an
    /// input stream is), and a **refusal** on the output side — see
    /// [`fsp_output_stream_option_refusal`].
    pub read: bool,
    /// `StandardOpenOption.WRITE`. Implied on the output side and a **refusal**
    /// on the input side — see [`fsp_input_stream_option_refusal`].
    pub write: bool,
    /// `StandardOpenOption.CREATE` (exactly; `CREATE_NEW` is [`Self::create_new`]).
    /// Read by `newFileChannel` (JIT round 22 wave 2, lane sync, W3L1-4).
    pub create: bool,
    /// `StandardOpenOption.TRUNCATE_EXISTING`. Read by `newFileChannel`.
    pub truncate_existing: bool,
    /// `StandardOpenOption.DELETE_ON_CLOSE` (JIT round 22 wave 3, lane sync).
    /// Set only by [`p57_scan_open_options_by_identity`]; `newFileChannel`
    /// unlinks the file right after the open, as `UnixChannelFactory` does.
    pub delete_on_close: bool,
}

/// JIT round 22 wave 3, lane sync (review `r22-review-w3-memory-on-throws-sync`
/// item 2; under `CRATONVM_COMPAT_OPEN_OPTIONS_BY_NAME`): the option set of a
/// `newFileChannel` call read the way `UnixChannelFactory.Flags.toFlags` reads
/// it -- by IDENTITY. Each element (through the set's own `toArray()`) must be
/// a `java.nio.file.StandardOpenOption` constant, matched by its exact enum
/// name, or `LinkOption.NOFOLLOW_LINKS`; a `null` element is the JDK's
/// `NullPointerException`, anything else its
/// `UnsupportedOperationException("<option> not supported")` (the option's own
/// `toString()`, as the JDK's string concatenation renders it). `SPARSE`,
/// `SYNC` and `DSYNC` are accepted (the JDK maps them to open flags the
/// fd-table opener does not take; no Java-visible answer differs on a local
/// file system). A null set reads as empty.
pub fn p57_scan_open_options_by_identity(
    ctx: &mut dyn NativeContext,
    set: Option<ObjectRef>,
) -> Result<P57OpenFlags, MethodCallFailed> {
    let mut flags = P57OpenFlags::default();
    let Some(set) = set else {
        return Ok(flags);
    };
    let arr = match ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[])? {
        Some(Value::Object(Some(arr))) => arr,
        _ => return Ok(flags),
    };
    let len = ctx.array_length(arr);
    // `toString()` below runs Java: the array is read back through its pin.
    let arr_pin = ctx.pin_native_root(arr);
    let mut refusal: Option<MethodCallFailed> = None;
    for i in 0..len {
        let arr_now = ctx.read_native_pin(arr_pin, arr);
        let opt = match ctx.get_array_element(arr_now, i) {
            Value::Object(Some(o)) => o,
            _ => {
                refusal = Some(RuntimeError::NullPointerException { message: None }.into());
                break;
            }
        };
        let class = ctx.class_name_of_id(ctx.class_id_of_object(opt)).unwrap_or_default();
        let name = match ctx.get_field_by_name(opt, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
        let known = match (class.as_str(), name.as_deref()) {
            ("java/nio/file/StandardOpenOption", Some(n)) => match n {
                "READ" => {
                    flags.read = true;
                    true
                }
                "WRITE" => {
                    flags.write = true;
                    true
                }
                "APPEND" => {
                    flags.append = true;
                    true
                }
                "TRUNCATE_EXISTING" => {
                    flags.truncate_existing = true;
                    true
                }
                "CREATE" => {
                    flags.create = true;
                    true
                }
                "CREATE_NEW" => {
                    flags.create_new = true;
                    true
                }
                "DELETE_ON_CLOSE" => {
                    flags.delete_on_close = true;
                    true
                }
                "SPARSE" | "SYNC" | "DSYNC" => true,
                _ => false,
            },
            ("java/nio/file/LinkOption", Some("NOFOLLOW_LINKS")) => {
                flags.nofollow = true;
                true
            }
            _ => false,
        };
        if !known {
            let text = match ctx.invoke_virtual(opt, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                Ok(_) => Some("null".to_string()),
                Err(e) => {
                    refusal = Some(e);
                    break;
                }
            };
            refusal = Some(
                RuntimeError::UnsupportedOperationException {
                    message: format!("{} not supported", text.unwrap_or_default()),
                }
                .into(),
            );
            break;
        }
    }
    ctx.unpin_native_roots(arr_pin);
    match refusal {
        Some(e) => Err(e),
        None => Ok(flags),
    }
}



/// Record one option NAME in `flags` (exact, ASCII case-insensitive, as the
/// array arm always compared). An unknown name records nothing.
pub fn p57_note_open_option_name(flags: &mut P57OpenFlags, n: &str) {
    let n = n.trim();
    if n.eq_ignore_ascii_case("APPEND") {
        flags.append = true;
    } else if n.eq_ignore_ascii_case("CREATE_NEW") {
        flags.create_new = true;
    } else if n.eq_ignore_ascii_case("CREATE") {
        flags.create = true;
    } else if n.eq_ignore_ascii_case("NOFOLLOW_LINKS") {
        flags.nofollow = true;
    } else if n.eq_ignore_ascii_case("READ") {
        flags.read = true;
    } else if n.eq_ignore_ascii_case("WRITE") {
        flags.write = true;
    } else if n.eq_ignore_ascii_case("TRUNCATE_EXISTING") {
        flags.truncate_existing = true;
    }
}

/// The option names of a collection's `toString()` (`AbstractCollection`'s
/// `"[A, B]"` shape): the bracketed list split on `,`, each name trimmed.
pub fn p57_open_option_names_of_set_text(text: &str) -> impl Iterator<Item = &str> {
    let t = text.trim();
    let t = t.strip_prefix('[').unwrap_or(t);
    let t = t.strip_suffix(']').unwrap_or(t);
    t.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// The refusal `FileSystemProvider.newInputStream` owes an option list that
/// asks for writing:
///
/// ```java
/// for (OpenOption opt: options) {
///     if (opt == StandardOpenOption.APPEND || opt == StandardOpenOption.WRITE)
///         throw new UnsupportedOperationException("'" + opt + "' not allowed");
/// }
/// ```
///
/// — "All `OpenOption` values except for `APPEND` and `WRITE` are allowed."
///
/// Note the type: `UnsupportedOperationException` here and
/// `IllegalArgumentException` in [`fsp_output_stream_option_refusal`], for the
/// mirror-image mistake. That is not an inconsistency to tidy up; it is what the
/// two JDK methods do, and a caller distinguishing them by `catch` clause sees
/// the difference.
///
/// Accepting these silently was the same defect as the scanner reading only
/// `nofollow`: an unrecognised or illegal option was taken as consent, so
/// `Files.newInputStream(p, WRITE)` handed back a read-only stream and the
/// caller's first write failed somewhere unrelated.
pub fn fsp_input_stream_option_refusal(flags: P57OpenFlags) -> Option<MethodCallFailed> {
    // The JDK reports whichever comes first in the caller's array; this scan
    // has already collapsed the order, so APPEND is reported when both are
    // present. Only the type and the fact of the refusal are load-bearing.
    let bad = if flags.append {
        "APPEND"
    } else if flags.write {
        "WRITE"
    } else {
        return None;
    };
    Some(
        RuntimeError::UnsupportedOperationException {
            message: format!("'{bad}' not allowed"),
        }
        .into(),
    )
}

/// The refusal `FileSystemProvider.newOutputStream` owes `READ`:
/// `if (opt == StandardOpenOption.READ) throw new IllegalArgumentException("READ not allowed");`
///
/// A *different* exception type from its sibling, deliberately — see
/// [`fsp_input_stream_option_refusal`].
pub fn fsp_output_stream_option_refusal(flags: P57OpenFlags) -> Option<MethodCallFailed> {
    if !flags.read {
        return None;
    }
    Some(
        RuntimeError::IllegalArgumentException {
            message: "READ not allowed".to_string(),
        }
        .into(),
    )
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
pub fn fsp_scan_open_options(
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
        // gc-common w20-d: an option without a String in slot 0 is asked
        // `toString()` (Java), and the next iteration read the array through
        // its address from before that call. Pinned, read back per element.
        let arr_pin = ctx.pin_native_root(obj);
        for i in 0..arr_len {
            let arr = ctx.read_native_pin(arr_pin, obj);
            let opt = match ctx.get_array_element(arr, i) {
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
                // Exact names, as this arm always compared (JIT round 22 wave 2,
                // lane sync: one helper, now also CREATE / TRUNCATE_EXISTING).
                p57_note_open_option_name(&mut flags, &n);
            }
        }
        ctx.unpin_native_roots(arr_pin);
        return flags;
    }
    // Set: rely on toString() — `HashSet.toString()` yields `[APPEND, WRITE]`
    // etc. Substring match is robust enough and avoids invoking iterator().
    // NB none of the five tokens is a substring of another StandardOpenOption
    // or LinkOption name (in particular `TRUNCATE_EXISTING` does not contain
    // `CREATE`, and no name contains `READ` or `WRITE` as a part), so the
    // substring test cannot over-match. Re-check that claim before adding a
    // sixth token — it is the property the whole branch rests on.
    if let Ok(Some(Value::Object(Some(s)))) =
        ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[])
    {
        if let Some(n) = ctx.read_string(s) {
            // JIT round 22 wave 2, lane sync (W3L1-4): each listed name,
            // exactly -- never a substring of the whole text.
            for name in p57_open_option_names_of_set_text(&n) {
                p57_note_open_option_name(&mut flags, name);
            }
            return flags;
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
pub const P57_NOFOLLOW_ELOOP_MESSAGE: &str =
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
pub fn p57_nofollow_reject(path: &str) -> Option<MethodCallFailed> {
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
/// docs/feature-designs/native-dispatch-architecture.md.
///
/// `path_index` is where the `Path` argument sits: 0 for the `Files` statics,
/// 1 for the `FileSystemProvider` instance method (whose slot 0 is `this`).
pub fn fsp_new_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    path_index: usize,
) -> MethodCallResult {
    // The option array is read from `args` after the Path: current args
    // (gc-common w21-b).
    let (p, args) = p57_read_path_arg(ctx, args, path_index)?;
    let args = &*args;
    // ONE walk of the option list, every verdict taken from it. The scanner used
    // to be asked only about `nofollow`, so an option list was read and then
    // three quarters of it discarded — which is how `WRITE` came to be accepted
    // here. Options sit one slot past the Path.
    let opts = fsp_scan_open_options(ctx, args.get(path_index + 1).copied());
    // Ahead of everything else, including the VFS branch: the JDK's check is the
    // first statement of `FileSystemProvider.newInputStream`, before any path is
    // resolved or any descriptor reserved, so a refusal leaves nothing behind
    // and does not depend on the file existing.
    if let Some(refused) = fsp_input_stream_option_refusal(opts) {
        return Err(refused);
    }
    // Jar/VFS entries have no file descriptor to hand out: they are already
    // decoded in memory and bounded by the entry size, so a snapshot is both
    // correct and the only option there.
    if let Some(read) = vfs_read(&p) {
        return match read {
            Ok(data) => files_byte_array_input_stream(ctx, &data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(p57_no_such_file(ctx, &p)?),
            Err(e) => Err(p57_io_error(&e)),
        };
    }
    // NOFOLLOW_LINKS is legal on the READ side too (`O_RDONLY|O_NOFOLLOW` still
    // ELOOPs), and callers use it to be sure they read the file they named and
    // not whatever a link now points at.
    if opts.nofollow {
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
                std::io::ErrorKind::NotFound => p57_no_such_file(ctx, &p)?,
                std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, &p)?,
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
    let stream = try_alloc_concurrent_synthetic(ctx, "java/io/FileInputStream", 4)?;
    // Pin across the allocations below — each can trigger a moving young GC
    // that relocates the fresh stream (native stale-local family).
    //
    // gc-common w20-d: the FileDescriptor and the path String are rooted too
    // and read back after the last allocation; only the stream was, so the
    // descriptor (held across two allocations) and the String (across one)
    // were stored through their pre-collection addresses.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let stream_h = scope.root(stream);
    let fd_h = match scope.new_object("java/io/FileDescriptor") {
        Ok(Some(Value::Object(Some(fd_obj)))) => scope.root(fd_obj),
        _ => {
            let _ = scope.fd_table().close(fd);
            return Err(RuntimeError::IOException {
                message: format!("newInputStream({p}): could not allocate a FileDescriptor"),
            }
            .into());
        }
    };
    let path_str = scope.create_string(&p);
    let path_h = scope.root(path_str);
    let close_lock = scope.new_object("java/lang/Object");
    let stream = scope.get(&stream_h);
    let fd_obj = scope.get(&fd_h);
    let path_str = scope.get(&path_h);
    // Platform spelling (`CRATONVM_FD_FIELDS_PER_PLATFORM`, JIT r23 w1 L6).
    let (fd_field, handle_field) = cratonvm_native_io::nio_native::descriptor_fields_for_id(
        fd,
        cratonvm_native_io::nio_native::fd_fields_per_platform_enabled(),
    );
    scope.set_field_by_name(fd_obj, "fd", Value::Int(fd_field));
    scope.set_field_by_name(fd_obj, "handle", Value::Long(handle_field));
    scope.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    // `FileInputStream.path` backs `getChannel()`/diagnostics on the real JDK.
    scope.set_field_by_name(stream, "path", Value::Object(Some(path_str)));
    if let Ok(Some(lock @ Value::Object(Some(_)))) = close_lock {
        scope.set_field_by_name(stream, "closeLock", lock);
    }
    scope.set_field_by_name(stream, "closed", Value::Int(0));
    // Belt-and-braces for legacy callers that read instance slot 0 directly.
    scope.set_field(stream, 0, Value::Object(Some(fd_obj)));
    Ok(Some(Value::Object(Some(stream))))
}

/// A `ByteArrayInputStream` over `data` — the in-memory stream shape used for
/// VFS (jar) entries, which have no file descriptor to stream from.
pub fn files_byte_array_input_stream(ctx: &mut dyn NativeContext, data: &[u8]) -> MethodCallResult {
    let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
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
pub fn fsp_new_output_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The option array is read from `args` after the Path: current args
    // (gc-common w21-b).
    let (p, args) = p57_read_path_arg(ctx, args, 1)?;
    let args = &*args;
    if p.is_empty() {
        return Err(RuntimeError::IOException {
            message: "newOutputStream: null path".to_string(),
        }
        .into());
    }
    let flags = fsp_scan_open_options(ctx, args.get(2).copied());
    // `READ` is an `IllegalArgumentException` here, and the mirror-image mistake
    // on the input side is an `UnsupportedOperationException` — the JDK means
    // the difference. Checked before the open, like the JDK's own check, so a
    // refusal creates and truncates nothing.
    if let Some(refused) = fsp_output_stream_option_refusal(flags) {
        return Err(refused);
    }
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
            // EISDIR is its own row: on Linux, opening a directory for
            // output fails with "Is a directory", and HotSpot reports that as
            // a `FileSystemException` carrying the path and the reason.
            // MEASURED: this VM answered a bare `IOException`, which is the
            // supertype every other failure also uses, so a caller could not
            // tell "you named a directory" from "the disk is full".
            return Err(match e.kind() {
                std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, &p)?,
                std::io::ErrorKind::NotFound => p57_no_such_file(ctx, &p)?,
                std::io::ErrorKind::IsADirectory => {
                    p57_filesystem_exception(ctx, &p, None, "Is a directory")?
                }
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
    // THE CONSTRUCTOR NEVER RUNS, so its instance initialisers have to be
    // reproduced here — exactly as `fsp_new_input_stream` does a hundred lines
    // above, with the comment that says why. This is the TWIN of that repair,
    // and it was missing: the input side set `closeLock`/`path`/`closed` and
    // the output side set only `fd`.
    //
    // `java.io.FileOutputStream.close()` opens `synchronized (closeLock)`, so a
    // null `closeLock` is
    //
    //   NullPointerException: Cannot enter synchronized block because
    //                         "this.closeLock" is null
    //       at java/io/FileOutputStream.close(FileOutputStream.java:383)
    //       at java/nio/file/Files.copy(Files.java:2865)
    //
    // — on an ordinary `Files.copy` / `Files.newOutputStream` inside a
    // try-with-resources. It is invisible TODAY only because
    // `FileOutputStream.close()` is itself natively overridden and never
    // reaches that bytecode. MEASURED with
    // `CRATONVM_ENFORCE_NATIVE_SHADOW=java/io/File`, which makes the overrides
    // yield: `probes/L4FilesSweep.java` died 119 rows early on exactly this.
    //
    // So it is latent, and it is the kind of latent that turns a future
    // retirement of the `FileOutputStream` shadows from free into a crash. Any
    // real-JDK-bytecode path that reaches `close()` finds it too.
    let fos = try_alloc_concurrent_synthetic(ctx, "java/io/FileOutputStream", 4)?;
    // Pin across the allocations below — each can trigger a moving young GC
    // that relocates the fresh stream (native stale-local family).
    //
    // gc-common w20-d: the FileDescriptor and the path String are rooted too
    // (the descriptor was held raw across two allocations, the String across
    // one), and the scope also releases the roots on the descriptor's `?`
    // exit, which leaked the stream's pin.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let fos_h = scope.root(fos);
    let fd_obj = try_alloc_concurrent_synthetic(&mut *scope, "java/io/FileDescriptor", 4)?;
    let fd_h = scope.root(fd_obj);
    let path_str = scope.create_string(&p);
    let path_h = scope.root(path_str);
    let close_lock = scope.new_object("java/lang/Object");
    let fos = scope.get(&fos_h);
    let fd_obj = scope.get(&fd_h);
    let path_str = scope.get(&path_h);
    // Platform spelling (`CRATONVM_FD_FIELDS_PER_PLATFORM`, JIT r23 w1 L6).
    let (fd_field, handle_field) = cratonvm_native_io::nio_native::descriptor_fields_for_id(
        fd,
        cratonvm_native_io::nio_native::fd_fields_per_platform_enabled(),
    );
    scope.set_field_by_name(fd_obj, "fd", Value::Int(fd_field));
    scope.set_field_by_name(fd_obj, "handle", Value::Long(handle_field));
    scope.set_field_by_name(fos, "fd", Value::Object(Some(fd_obj)));
    // `path` backs `getChannel()` and the JDK's own diagnostics; `append` is
    // read by `FileOutputStream.getChannel()`; `closed` must start false.
    scope.set_field_by_name(fos, "path", Value::Object(Some(path_str)));
    scope.set_field_by_name(fos, "append", Value::Int(i32::from(append)));
    if let Ok(Some(lock @ Value::Object(Some(_)))) = close_lock {
        scope.set_field_by_name(fos, "closeLock", lock);
    }
    scope.set_field_by_name(fos, "closed", Value::Int(0));
    // Belt-and-braces for legacy callers that read instance slot 0 directly.
    scope.set_field(fos, 0, Value::Object(Some(fd_obj)));
    Ok(Some(Value::Object(Some(fos))))
}

/// `Files.write(Path, byte[], OpenOption...)`.
///
/// Registered twice (here and in `register_p71_files_bridge`) for the same
/// (class, name, descriptor); registration is last-writer-wins, so both point
/// at this one function and it no longer matters which runs last.
pub fn files_write_bytes_impl(
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
pub fn files_write_string_impl(
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
/// nio-write-ignores-nofollow-links-symlink-20260804-FIXED.md.
pub fn p57_files_write_bytes(
    ctx: &mut dyn NativeContext,
    path_obj: ObjectRef,
    bytes: &[u8],
    options: Option<Value>,
) -> MethodCallResult {
    // The Path read (a foreign Path's `toString()`) and the option scan (an
    // option's `toString()`) can both run Java and therefore move `path_obj`
    // (this method returns it) and the option array (scanned after the read).
    // Both are pinned across both (gc-common w21-b: the array was held raw
    // across the Path read).
    let path_pin = ctx.pin_native_root(path_obj);
    let opts_pin = match options {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let mut path_now = path_obj;
    let p = p57_read_path(ctx, &mut path_now);
    let options = match opts_pin {
        Some((h, o)) => Some(Value::Object(Some(ctx.read_native_pin(h, o)))),
        None => options,
    };
    let flags = fsp_scan_open_options(ctx, options);
    let path_obj = ctx.read_native_pin(path_pin, path_obj);
    ctx.unpin_native_roots(path_pin);

    // `Files.write(path, bytes, READ)` is an `IllegalArgumentException`, and
    // `fsp_output_stream_option_refusal` is the check that says so — the
    // sibling `newOutputStream` already ran it and this path did not, so the
    // same option was refused through one door and accepted through the other.
    // MEASURED: this VM opened the file for WRITING and truncated it, which is
    // the opposite of what READ asks for.
    if let Some(refused) = fsp_output_stream_option_refusal(flags) {
        return Err(refused);
    }
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
                std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, &p)?,
                std::io::ErrorKind::NotFound => p57_no_such_file(ctx, &p)?,
                std::io::ErrorKind::IsADirectory => {
                    p57_filesystem_exception(ctx, &p, None, "Is a directory")?
                }
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

/// The `std::io::Error` -> Java exception mapping every `java.nio.file` entry
/// point owes its caller.
///
/// `Files.copy` and `Files.move` both ended in
///
/// ```rust,ignore
/// Err(e) => Err(RuntimeError::IllegalStateException { message: format!("IOException: {e}") })
/// ```
///
/// which is wrong twice over. `IllegalStateException` is **not an
/// `IOException`**, so `catch (IOException)` — the handler every caller of
/// these two methods is REQUIRED to write, because both declare `throws
/// IOException` — does not match it, and the exception unwinds past the code
/// written to handle it. And the specific subtype is the contract: MEASURED,
///
/// ```text
///   Files.copy(missing, t)      HotSpot NoSuchFileException   CratonVM IllegalStateException
///   Files.move(s, missingdir/x) HotSpot NoSuchFileException   CratonVM IllegalStateException
/// ```
///
/// The message it built ("IOException: No such file or directory (os error 2)")
/// shows the intent — it knew which exception it meant and could not spell it.
pub fn p57_fs_error(
    ctx: &mut dyn NativeContext,
    e: &std::io::Error,
    file: &str,
) -> MethodCallFailed {
    let built = match e.kind() {
        std::io::ErrorKind::NotFound => p57_no_such_file(ctx, file),
        std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, file),
        std::io::ErrorKind::AlreadyExists => return p57_file_already_exists(ctx, file),
        std::io::ErrorKind::DirectoryNotEmpty => p57_directory_not_empty(ctx, file),
        std::io::ErrorKind::NotADirectory => p57_not_directory(ctx, file),
        std::io::ErrorKind::IsADirectory => {
            p57_filesystem_exception(ctx, file, None, "Is a directory")
        }
        _ => return p57_io_error(e),
    };
    match built {
        Ok(f) => f,
        Err(f) => f,
    }
}

/// A `java.nio.charset.MalformedInputException` for a decode that failed.
///
/// `Files.readString` / `readAllLines` / `lines` decode with a CharsetDecoder
/// configured to REPORT, so malformed input raises rather than substituting
/// U+FFFD. This VM read every file with `String::from_utf8_lossy`, so
/// `Files.readString(<two bytes of broken UTF-8>)` answered a String
/// containing replacement characters and the caller could not tell a corrupt
/// file from a file that genuinely contains them. MEASURED: HotSpot
/// `MalformedInputException`, this VM no-throw.
///
/// The class's only constructor takes the input length, and `getMessage()`
/// renders it, so it is passed rather than a text. Falls back to the caller's
/// own error when the class cannot be resolved.
///
/// Round 15 wave 4, lane jdkonly4
/// (`r15w3-jdkonly3-residual-new-object-init-builders-patch-FIXED-20260930.md` #2):
/// `NativeHeapAccess::construct_throwable`. This was `new_object` + an
/// `invoke` of `<init>` with the exception held RAW across the constructor
/// (which captures a stack trace, i.e. allocates) and that pre-constructor
/// address thrown -- stale after a moving collection -- and a constructor
/// failure ignored. A constructor that throws now propagates its throwable.
pub fn p57_malformed_input(
    ctx: &mut dyn NativeContext,
    input_length: i32,
) -> Option<MethodCallFailed> {
    match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/nio/charset/MalformedInputException",
        "(I)V",
        &[Value::Int(input_length)],
    ) {
        Ok(Some(exc)) => Some(MethodCallFailed::ExceptionThrown(exc)),
        Err(thrown) => Some(thrown),
        Ok(None) => None,
    }
}

/// Decode `bytes` with the charset a `Files.readString`/`readAllLines`
/// overload was actually given.
///
/// **The `(Path, Charset)` overloads ignored the charset**, and the body said
/// so (`// Ignore charset, always UTF-8`). Before this lane that was a silent
/// wrong answer: the bytes were decoded as UTF-8 with replacement, so
/// `Files.readString(p, ISO_8859_1)` on non-UTF-8 bytes returned U+FFFD where
/// the JDK returns the latin-1 characters. Part one of this lane made the
/// UTF-8 decode STRICT, which fixed the default overload and turned the
/// charset overloads into a wrong REFUSAL instead of a wrong answer:
///
/// ```text
///   Files.readString(<0xC3 0x28>, ISO_8859_1)
///     HotSpot   a 2-character String        (latin-1 cannot fail)
///     CratonVM  MalformedInputException     <- after part one
/// ```
///
/// MEASURED with `L4TailSweep2.java`, where it killed the run 32
/// rows early. Both halves are the same root cause: the argument was never
/// read.
///
/// `Ok(Err(len))` is a malformed-input refusal carrying the offending length,
/// exactly as [`p57_read_to_string_strict`] reports it; `Err(())` means the
/// charset could not be identified and the caller should keep its previous
/// behaviour rather than guess.
///
/// The three single-byte charsets are decoded here because they cannot fail
/// and because they are what a program naming a charset almost always names;
/// anything else goes through the JDK's own `new String(byte[], Charset)`,
/// which is right for every charset this VM can load.
pub fn p57_decode_with_charset(
    ctx: &mut dyn NativeContext,
    bytes: &[u8],
    charset: Option<ObjectRef>,
) -> Result<Result<String, usize>, ()> {
    let Some(cs) = charset else { return Err(()) };
    let cid = ctx.class_id_of_object(cs);
    match ctx.class_name_arc_of_id(cid).as_deref() {
        // The abstract stand-in `install_charset` stamps at bootstrap means
        // UTF-8 here, exactly as it does for `PrintStream.charset()`.
        Some("sun/nio/cs/UTF_8") | Some("java/nio/charset/Charset") | None => {
            Ok(match String::from_utf8(bytes.to_vec()) {
                Ok(s) => Ok(s),
                Err(e) => Err(e.utf8_error().error_len().unwrap_or(1)),
            })
        }
        // Every byte is a character. A latin-1 decode CANNOT fail, which is
        // the row that caught this.
        Some("sun/nio/cs/ISO_8859_1") => Ok(Ok(bytes.iter().map(|&b| b as char).collect())),
        // US-ASCII REPORTS. `Files.readString`/`readAllLines` decode with a
        // `CharsetDecoder` left on its default action, which is REPORT, not
        // REPLACE -- so a byte above 0x7F is a `MalformedInputException` and
        // NOT a U+FFFD. MEASURED against HotSpot, which is also how the first
        // version of this arm (a replacing one) was caught: the oracle refused
        // the row this VM answered.
        //
        // The three single-byte charsets therefore answer three DIFFERENT ways
        // to the same two bytes -- latin-1 cannot fail, UTF-8 and US-ASCII
        // both refuse, and only latin-1 returns a string.
        Some("sun/nio/cs/US_ASCII") => Ok(match bytes.iter().position(|&b| b >= 0x80) {
            Some(_) => Err(1),
            None => Ok(bytes.iter().map(|&b| b as char).collect()),
        }),
        // APPROXIMATION, stated rather than hidden: `new String(byte[],
        // Charset)` decodes with REPLACE, while `Files.readString` uses a
        // decoder on REPORT. For any charset outside the three above, a
        // malformed sequence therefore comes back as U+FFFD here where HotSpot
        // would refuse. The alternative is driving a real `CharsetDecoder`
        // through four re-entrant calls per read; this is the cheaper half of
        // that trade and it is right for every well-formed input.
        Some(_) => {
            use cratonvm_types::ArrayElementType;
            // gc-common w20-d: the charset was held raw across both
            // allocations, the array across the String's, and the String was
            // read after `<init>` (Java, which allocates the value array).
            // All three are rooted and read back.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let cs_h = scope.root(cs);
            let arr = scope.new_array(ArrayElementType::Byte, bytes.len());
            scope.write_byte_array_from(arr, 0, bytes);
            let arr_h = scope.root(arr);
            let s = match scope.new_object("java/lang/String") {
                Ok(Some(Value::Object(Some(s)))) => s,
                _ => return Err(()),
            };
            let s_h = scope.root(s);
            let arr = scope.get(&arr_h);
            let cs = scope.get(&cs_h);
            let built = scope.invoke(
                "java/lang/String",
                "<init>",
                "([BLjava/nio/charset/Charset;)V",
                &[
                    Value::Object(Some(s)),
                    Value::Object(Some(arr)),
                    Value::Object(Some(cs)),
                ],
            );
            let s = scope.get(&s_h);
            match built {
                Ok(_) => match scope.read_string(s) {
                    Some(text) => Ok(Ok(text)),
                    None => Err(()),
                },
                Err(_) => Err(()),
            }
        }
    }
}

/// [`p57_read_to_string`] with the JDK's REPORT action instead of REPLACE.
///
/// Returns `Err(None)` for an I/O failure the caller must map itself, and
/// `Err(Some(len))` when the bytes are not valid UTF-8 — `len` being the length
/// of the offending sequence, which is what `MalformedInputException` carries.
/// The bytes of a path, with the same jar/jrt awareness the string readers
/// have. Split out so a charset-aware caller can decode them itself.
pub fn p57_read_bytes(p: &str) -> Result<Vec<u8>, std::io::Error> {
    match vfs_read(p) {
        Some(r) => r,
        None => std::fs::read(p),
    }
}

pub fn p57_read_to_string_strict(p: &str) -> Result<Result<String, usize>, std::io::Error> {
    let bytes = match vfs_read(p) {
        Some(r) => r?,
        None => std::fs::read(p)?,
    };
    match String::from_utf8(bytes) {
        Ok(s) => Ok(Ok(s)),
        Err(e) => {
            // `error_len()` is `None` for an unexpected end of input, which the
            // JDK reports with length 1.
            Ok(Err(e.utf8_error().error_len().unwrap_or(1)))
        }
    }
}

/// Build a `java.nio.file` exception `class(file)` -- or `class(file, other,
/// reason)` when either of those is given -- through its CONSTRUCTOR
/// ([`crate::throwable_mint::construct_or_mint`]) and return it as thrown.
///
/// Round 15 wave 3, lane jdkonly2
/// (`r15w2-jdkonly-native-minted-throwables-skip-constructor-FIXED-20260930.md`):
/// every builder below used to be a bare `try_alloc_concurrent_synthetic` plus
/// by-name writes, so `Throwable`'s declared initialisers never ran -- `cause`
/// null instead of `this`, `suppressedExceptions` null instead of the
/// sentinel, no stack trace -- and under `--real-jdk`, where `initCause` /
/// `addSuppressed` / `getStackTrace` are the JDK's bytecode, `initCause` threw
/// "Can't overwrite cause" and try-with-resources dropped its suppressed
/// exception. `FileSystemException(String file)` is exactly "file set, `other`
/// and `detailMessage` null" (the state those writes built by hand), and the
/// three-argument form passes the reason to `super(reason)`, so `detailMessage`
/// carries it. `mint` is the site's literal allocation, used only when the
/// constructor cannot build the object.
pub fn p57_path_exception(
    ctx: &mut dyn NativeContext,
    class: &str,
    mint: crate::throwable_mint::ThrowableMint,
    file: &str,
    other: Option<&str>,
    reason: Option<&str>,
) -> Result<MethodCallFailed, MethodCallFailed> {
    let file_obj = p57_exception_path_string(ctx, file);
    let file_pin = ctx.pin_native_root(file_obj);
    let other_obj = other.map(|o| p57_exception_path_string(ctx, o));
    let other_pin = other_obj.map(|o| ctx.pin_native_root(o));
    // The OS's explanation: a fresh string, as `UnixException.errorString`'s is.
    let reason_obj = reason.map(|r| ctx.create_string_uninterned(r));
    let file_obj = ctx.read_native_pin(file_pin, file_obj);
    let other_obj = match (other_obj, other_pin) {
        (Some(o), Some(pin)) => Some(ctx.read_native_pin(pin, o)),
        _ => None,
    };
    ctx.unpin_native_roots(file_pin);
    let file_v = Value::Object(Some(file_obj));
    let other_v = Value::Object(other_obj);
    let reason_v = Value::Object(reason_obj);
    let three = [file_v, other_v, reason_v];
    let (init_desc, init_args) = if other.is_some() || reason.is_some() {
        (
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            &three[..],
        )
    } else {
        ("(Ljava/lang/String;)V", &three[..1])
    };
    let exc = crate::throwable_mint::construct_or_mint(
        ctx,
        &crate::throwable_mint::ThrowableCtor {
            class,
            init_desc,
            init_args,
            mint,
            message: reason_v,
            fields: &[("file", file_v), ("other", other_v)],
            null_cause: false,
        },
    )?;
    Ok(MethodCallFailed::ExceptionThrown(exc))
}

/// Build a *typed* `java.nio.file.NoSuchFileException` for `path` and return it
/// wrapped as a thrown Java exception.
///
/// The thrown object carries the genuine `ClassId` — exception handlers that
/// `catch (NoSuchFileException)` (or any superclass: `FileSystemException`,
/// `IOException`, `Exception`) match it correctly. `FileSystemException.file`
/// carries the offending path; `FileSystemException.getMessage()` builds the
/// human message from that field, so `Throwable.detailMessage` stays null
/// (a populated one renders as `<path>: <path>`). Built by
/// [`p57_path_exception`], which runs the constructor.
pub fn p57_no_such_file(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    p57_path_exception(
        ctx,
        "java/nio/file/NoSuchFileException",
        |ctx| try_alloc_concurrent_synthetic(ctx, "java/nio/file/NoSuchFileException", 4),
        path,
        None,
        None,
    )
}

/// Build a *typed* `java.nio.file.AccessDeniedException` for `path` (mirrors
/// [`p57_no_such_file`]). `AccessDeniedException extends FileSystemException
/// extends IOException`, so the thrown object carries the genuine `ClassId` and
/// matches `catch (AccessDeniedException)` / `FileSystemException` / `IOException`.
/// Used when an OS open returns access-denied (e.g. opening a directory for
/// output on Windows) so the thrown type matches HotSpot.
pub fn p57_access_denied(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    p57_path_exception(
        ctx,
        "java/nio/file/AccessDeniedException",
        |ctx| try_alloc_concurrent_synthetic(ctx, "java/nio/file/AccessDeniedException", 4),
        path,
        None,
        None,
    )
}

/// Build a *typed* `java.nio.file.NotDirectoryException` for `path` (mirrors
/// [`p57_no_such_file`]).
///
/// `NotDirectoryException extends FileSystemException extends IOException`, so
/// the thrown object matches `catch (NotDirectoryException)` and every
/// supertype. Measured on Eclipse Adoptium 25.0.3+9 (2026-08-16):
///
/// ```text
/// Files.list(<regular file>)                       -> NotDirectoryException: <path>
/// provider.newDirectoryStream(<regular file>, all) -> NotDirectoryException: <path>
/// Files.newDirectoryStream(<regular file>)         -> NotDirectoryException: <path>
/// ```
///
/// The message is the path alone, which is what `FileSystemException.getMessage`
/// builds from the `file` field, so `detailMessage` is deliberately left null —
/// same convention as [`p57_no_such_file`], for the same reason (a populated
/// `detailMessage` renders as `<path>: <path>`).
pub fn p57_not_directory(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    p57_path_exception(
        ctx,
        "java/nio/file/NotDirectoryException",
        |ctx| try_alloc_concurrent_synthetic(ctx, "java/nio/file/NotDirectoryException", 4),
        path,
        None,
        None,
    )
}

/// The refusal every directory-listing entry point owes a path it cannot list.
///
/// **This is the fix for the largest fabricated-success row on the shipping
/// `java.nio.file` surface.** `newDirectoryStream`, `Files.list`, `Files.walk`
/// and `Files.find` all reached the host through
/// [`vfs_or_host_list`], whose host arm ends
///
/// ```text
/// match std::fs::read_dir(p) { Ok(rd) => …, Err(_) => vec![] }
/// ```
///
/// — so a directory that does not exist, a path that is a *regular file*, and a
/// directory the process may not read were all answered with an **empty
/// listing**. Nothing threw; the caller was told the directory is empty. That is
/// the worst shape in this species, because "empty" is a legal answer that every
/// scan/backup/clean loop accepts and acts on: a `Files.list(dir)` over a
/// mistyped path deletes nothing, copies nothing and reports success.
///
/// Measured on Eclipse Adoptium 25.0.3+9 (2026-08-16), all three entry points
/// agree:
///
/// ```text
/// Files.list(<missing>)      -> NoSuchFileException:   <path>
/// Files.list(<regular file>) -> NotDirectoryException: <path>
/// Files.walk(<missing>)      -> NoSuchFileException:   <path>   (at construction)
/// Files.find(<missing>, …)   -> NoSuchFileException:   <path>
/// provider.newDirectoryStream(<missing>, …)      -> NoSuchFileException
/// provider.newDirectoryStream(<regular file>, …) -> NotDirectoryException
/// ```
///
/// The classification is taken from `symlink_metadata`/`metadata` rather than
/// from `read_dir`'s `ErrorKind`, deliberately: `ErrorKind::NotADirectory` is a
/// recent addition and Windows reports the same condition as a raw OS error
/// code, so keying on it would make this check platform- and toolchain-
/// dependent. An explicit stat answers the same question on every host.
///
/// Returns `Some(exception)` when the listing must be refused and `None` when it
/// may proceed. `require_directory` is false for the callers that are allowed to
/// name a non-directory — `Files.walk`/`Files.find` accept a regular file and
/// yield exactly that one element (measured: `Files.walk(<file>).count() == 1`).
pub fn p57_dir_listing_refusal(
    ctx: &mut dyn NativeContext,
    path: &str,
    require_directory: bool,
) -> Result<Option<MethodCallFailed>, MethodCallFailed> {
    // jar:/jrt: namespaces are decoded, not stat-ed. `vfs_classify` is the one
    // authority there and it already distinguishes the three cases.
    if let Some(kind) = vfs_classify(path) {
        return Ok(match kind {
            JarFsKind::Absent => Some(p57_no_such_file(ctx, path)?),
            JarFsKind::File if require_directory => Some(p57_not_directory(ctx, path)?),
            _ => None,
        });
    }
    // `symlink_metadata` first so a DANGLING symlink is reported as the
    // NoSuchFileException the JDK raises (an `lstat` succeeds on it, a `stat`
    // does not) rather than being mistaken for "not a directory".
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(Some(p57_no_such_file(ctx, path)?));
    }
    match std::fs::metadata(path) {
        // A link whose target is gone: the JDK's open fails, and it fails with
        // the missing-file answer.
        Err(_) => Ok(Some(p57_no_such_file(ctx, path)?)),
        Ok(m) if require_directory && !m.is_dir() => Ok(Some(p57_not_directory(ctx, path)?)),
        Ok(_) => Ok(None),
    }
}

/// The refusal `Files.walk` / `Files.find` owe a negative `maxDepth`.
///
/// Measured on Eclipse Adoptium 25.0.3+9 (2026-08-16):
///
/// ```text
/// Files.walk(dir, -1)          -> IllegalArgumentException: 'maxDepth' is negative
/// Files.find(dir, -1, (p,a)->…) -> IllegalArgumentException: 'maxDepth' is negative
/// ```
///
/// The quoting is HotSpot's own (`FileTreeIterator` builds the message as
/// `"'maxDepth' is negative"`), so it is transcribed rather than derived.
///
/// This is not a cosmetic row. Both call sites read the depth as
/// `Some(Value::Int(n)) if *n >= 0 => *n as usize, _ => usize::MAX`, so a
/// **negative** depth — the one input the JDK refuses outright — was silently
/// promoted to an **unbounded** walk. The caller asked for a bounded traversal,
/// was given the opposite, and got no diagnostic.
pub fn p57_max_depth_refusal(depth: Option<&Value>) -> Option<MethodCallFailed> {
    match depth {
        Some(Value::Int(n)) if *n < 0 => Some(
            RuntimeError::IllegalArgumentException {
                message: "'maxDepth' is negative".to_string(),
            }
            .into(),
        ),
        _ => None,
    }
}

/// Build a *typed* `java.nio.channels.ClosedChannelException`.
///
/// Every `FileChannel` operation below is specified to raise this — not a bare
/// `IOException` — once the channel is closed ("@throws ClosedChannelException
/// If this channel is closed", `FileChannel.java`, on `position`, `position(J)`,
/// `size`, `truncate`, `read`, `write`, `transferTo`, `transferFrom` and
/// `force`). The type is what callers key on: a retry/reopen branch guarded by
/// `catch (ClosedChannelException)` does not match a supertype instance, so
/// reporting `IOException` deletes the branch the same way answering `-1` did.
///
/// It carries no payload — the JDK's constructor is the no-arg one and the
/// class declares no fields — so this runs the real `<init>()V` rather than
/// populating a field the way [`p57_no_such_file`] does. `new_object` failing
/// (no such class in a synthetic image) falls back to the `IOException` this
/// used to be, which is strictly no worse than before.
pub fn p57_closed_channel(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/nio/channels/ClosedChannelException",
        "()V",
        &[],
        || {
            RuntimeError::IOException {
                message: "Channel closed".into(),
            }
            .into()
        },
    )
}

/// The exception `java.io.BufferedWriter.ensureOpen()` raises, verbatim:
/// `if (out == null) throw new IOException("Stream closed")`. Every write,
/// `newLine` and `flush` in the real class opens with that check, so a
/// BufferedWriter whose `out` is null must refuse rather than accept the
/// characters and drop them.
/// Only the `synthetic-jdk` `BufferedWriter` arms raise this; the shipping
/// builds get `ensureOpen()`'s own `IOException` from the real class.
#[allow(dead_code)]
pub fn bw_stream_closed() -> MethodCallFailed {
    RuntimeError::IOException {
        message: "Stream closed".into(),
    }
    .into()
}

/// `java.io.UTFDataFormatException`, the refusal `DataOutput.writeUTF` owes an
/// over-long string: "First, the total number of bytes needed to represent all
/// the characters of `s` is calculated. If this number is larger than `65535`,
/// then a `UTFDataFormatException` is thrown."
///
/// Built through the real class so `catch (UTFDataFormatException)` matches. It
/// extends `IOException`, so the fallback for an image that does not declare it
/// is a weakening of the type rather than a different contract.
///
/// Round 15 wave 4, lane jdkonly4
/// (`r15w3-jdkonly3-residual-new-object-init-builders-patch-FIXED-20260930.md` #3):
/// `throwable_mint::construct_thrown` -- a constructor that threw is
/// propagated, not ignored with the half-built object thrown -- and the
/// message is a fresh string (it was interned into the never-pruned pool).
pub fn raf_utf_data_format(ctx: &mut dyn NativeContext, message: &str) -> MethodCallFailed {
    let msg = ctx.create_string_uninterned(message);
    crate::throwable_mint::construct_thrown(
        ctx,
        "java/io/UTFDataFormatException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
        || {
            RuntimeError::IOException {
                message: format!("UTFDataFormatException: {message}"),
            }
            .into()
        },
    )
}

/// Remove a single filesystem entry, choosing `remove_dir` vs `remove_file` the
/// way the JDK's `Files.delete` does: on the entry's OWN type.
///
/// This used to branch on `Path::is_dir()`, which resolves symbolic links — so
/// deleting a link to a directory called `remove_dir` on the link, which fails
/// with ENOTDIR and (because the error was discarded) left the link on disk.
/// `FileSystemUtils.deleteRecursively` on such a link therefore silently did
/// nothing, stranding it for every later caller.
pub fn p57_delete_path(path: &str) {
    let _ = p57_delete_path_checked(path);
}

/// The same removal, but reporting what happened.
///
/// `Files.delete` used to discard every error, so a delete that did not happen
/// returned normally — including the case H2 depends on: on Windows,
/// `DeleteFileW` refuses a file carrying `FILE_ATTRIBUTE_READONLY`, and
/// `FilePathDisk.delete` catches that `AccessDeniedException`, clears
/// `dos:readonly` and retries. Silently deleting the file instead meant that
/// recovery branch never ran, and a program relying on read-only protection got
/// no protection at all.
///
/// Rust's `std::fs::remove_file` is what made the difference invisible: on
/// Windows it CLEARS the read-only attribute and retries rather than failing,
/// deliberately, to give Unix semantics. That is the one case this has to
/// re-refuse by hand; every other error is just no longer thrown away.
pub fn p57_delete_path_checked(path: &str) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        if !md.is_dir() && md.permissions().readonly() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the file is marked read-only",
            ));
        }
    }
    if md.is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Turn a removal failure into the exception type the JDK raises for it.
pub fn p57_delete_error(
    ctx: &mut dyn NativeContext,
    path: &str,
    error: &std::io::Error,
) -> Result<MethodCallFailed, MethodCallFailed> {
    match error.kind() {
        std::io::ErrorKind::NotFound => Ok(p57_no_such_file(ctx, path)?),
        std::io::ErrorKind::PermissionDenied => Ok(p57_access_denied(ctx, path)?),
        _ if is_directory_not_empty(error) => Ok(p57_directory_not_empty(ctx, path)?),
        _ => p57_filesystem_exception(ctx, path, None, &error.to_string()),
    }
}

/// ENOTEMPTY (`ERROR_DIR_NOT_EMPTY` on Windows), matched on the raw OS code
/// rather than on `ErrorKind`, which reports it as `Uncategorized` on the
/// toolchains this tree builds with.
pub fn is_directory_not_empty(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    let code = Some(libc::ENOTEMPTY);
    #[cfg(windows)]
    let code = Some(WIN_ERROR_DIR_NOT_EMPTY);
    #[cfg(not(any(unix, windows)))]
    let code: Option<i32> = None;
    match code {
        Some(c) => error.raw_os_error() == Some(c),
        None => false,
    }
}

/// Build a *typed* `java.nio.file.DirectoryNotEmptyException`.
///
/// It used to be reported as a bare `FileSystemException` with the distinction
/// carried only in the reason text, on the reasoning that "we do not model the
/// subclass". But the subclass is the whole point of the type: a recursive
/// delete walks children exactly when it catches `DirectoryNotEmptyException`,
/// and `catch (DirectoryNotEmptyException)` does not match a supertype instance,
/// so that branch silently never ran. HotSpot on the same host raises
/// `DirectoryNotEmptyException` for `Files.delete` of a non-empty directory —
/// verified by `NioExcShape` against JDK 25.
///
/// The JDK's constructor takes only the directory and leaves the reason null,
/// which is why nothing is written to `detailMessage` here.
pub fn p57_directory_not_empty(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    p57_path_exception(
        ctx,
        "java/nio/file/DirectoryNotEmptyException",
        |ctx| try_alloc_concurrent_synthetic(ctx, "java/nio/file/DirectoryNotEmptyException", 4),
        path,
        None,
        None,
    )
}

/// Build a *typed* `java.nio.file.FileAlreadyExistsException` for the call
/// sites that used to allocate rather than run the real constructor.
///
/// This body was copied inline at each `ErrorKind::AlreadyExists` arm, which is
/// how `createDirectory` kept naming a forward-slash path on Windows after
/// every other builder had been fixed: a per-site conversion cannot reach a
/// site nobody remembers exists. [`p57_file_already_exists`] is the variant that
/// runs the class's real one-String constructor, kept separate because H2's own
/// bytecode catches that instance and reads its fields. Since round 15 wave 3
/// this one runs the constructor too ([`p57_path_exception`]); the name keeps
/// its `Result` shape for its callers.
pub fn p57_file_already_exists_synthetic(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    p57_path_exception(
        ctx,
        "java/nio/file/FileAlreadyExistsException",
        |ctx| try_alloc_concurrent_synthetic(ctx, "java/nio/file/FileAlreadyExistsException", 4),
        path,
        None,
        None,
    )
}

/// Build a *typed* `java.nio.file.NotLinkException` for `path` (mirrors
/// [`p57_no_such_file`]). Thrown by `readSymbolicLink` when the path exists but
/// is not a symbolic link — the JDK's contract, and what
/// `Files.readSymbolicLink`'s callers catch.
///
/// `reason` is the OS's explanation when the caller has one, and goes to
/// `detailMessage` for the same reason it does in [`p57_filesystem_exception`]:
/// `NotLinkException` inherits `FileSystemException`'s two fields and reads its
/// reason out of `Throwable`. HotSpot reports one here, so `None` is only for
/// callers that genuinely have no OS error to quote.
pub fn p57_not_link(
    ctx: &mut dyn NativeContext,
    path: &str,
    reason: Option<&str>,
) -> Result<MethodCallFailed, MethodCallFailed> {
    // Round 15 wave 3, lane jdkonly2: `NotLinkException(file, null, reason)`
    // through its constructor. The old body also RETURNED the reference it
    // read before the reason's `create_string` (the re-read was scoped to the
    // `if`), a stale local under a moving collection.
    p57_path_exception(
        ctx,
        "java/nio/file/NotLinkException",
        |ctx| try_alloc_concurrent_synthetic(ctx, "java/nio/file/NotLinkException", 4),
        path,
        None,
        reason,
    )
}

/// Build a *typed* `java.nio.file.FileSystemException` carrying the JDK's
/// three-part `(file, other, reason)` shape.
///
/// **The reason is not a field of this class.** `java.nio.file.FileSystemException`
/// declares exactly two instance fields, `file` and `other`; its constructor
/// passes the reason to `super(reason)` and `getReason()` returns
/// `Throwable.getMessage()`. Verified against the host JDK 25:
///
/// ```text
/// declaredFields: serialVersionUID, file, other        (no `reason`)
/// new FileSystemException("F","O","R")
///     getReason() = R   getMessage() = "F -> O: R"   detailMessage = R
/// new FileSystemException("F")
///     getReason() = null getMessage() = "F"          detailMessage = null
/// ```
///
/// So the reason goes to `detailMessage`. Writing it to a `"reason"` field
/// instead resolves nothing and is silently dropped — the by-name-write failure
/// mode — which left every `FileSystemException` CratonVM raised reaching Java
/// with `getReason() == null` and a message truncated to `"<file> -> <other>"`.
/// `getMessage()` is overridden by the real class and assembles the three parts
/// itself, so it must NOT be pre-rendered here.
///
/// This is the type the JDK raises for an OS-level link failure that is not one
/// of the specific subclasses — most visibly Windows' `ERROR_PRIVILEGE_NOT_HELD`
/// ("A required privilege is not held by the client"), which is what a symlink
/// creation gets on any Windows host without Developer Mode or an elevated
/// token. Reporting it as `UnsupportedOperationException` (the old behaviour)
/// made a *host* limitation look like a missing JDK feature — and dropping the
/// reason string put it right back, because the text naming the host gap was the
/// part being discarded.
///
/// The one-argument builders ([`p57_no_such_file`] and friends) correctly leave
/// `detailMessage` null: HotSpot likewise reports `getReason() == null` there.
pub fn p57_filesystem_exception(
    ctx: &mut dyn NativeContext,
    file: &str,
    other: Option<&str>,
    reason: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    p57_path_exception(
        ctx,
        "java/nio/file/FileSystemException",
        |ctx| try_alloc_concurrent_synthetic(ctx, "java/nio/file/FileSystemException", 4),
        file,
        other,
        Some(reason),
    )
}

/// Render a VM-internal path the way the platform's `Path.toString()` does,
/// for embedding in an exception's `file`/`other` field.
///
/// CratonVM stores paths in one internal form that uses `/` on every platform.
/// `java.nio.file.Path.toString()` converts back to the platform separator, but
/// the exception builders here were handing the *internal* string straight to
/// the exception, so on Windows every `java.nio.file` exception named
/// `C:/Users/...` where HotSpot names `C:\Users\...`. It is not symlink-specific
/// — `newByteChannel`, `createDirectory` and `delete` were all affected — and it
/// breaks any test that compares an exception message against a `Path`.
///
/// On Unix the separator already matches, so this is the identity function and
/// costs nothing.
/// A virtual-filesystem path (a zip/jar entry, or a runtime-image entry) is
/// exempt: entry names inside an archive are `/`-separated on every platform,
/// exactly as the JDK's own `zipfs`/`jrtfs` render them, so rewriting those
/// would introduce the divergence this is here to remove — and the encoding's
/// own structure would be corrupted along with it.
///
/// The implementation lives in `native-io` because that crate raises
/// `java.nio.file` exceptions too and cannot depend on this one (the dependency
/// runs the other way). One implementation, so the two crates cannot drift.
pub fn p57_exception_path(path: &str) -> std::borrow::Cow<'_, str> {
    cratonvm_native_io::nio_native::exception_path(path)
}

/// `native-io` cannot see [`JARFS_SENTINEL`], so it declares the same marker
/// itself. If either moves, this stops compiling instead of silently letting
/// archive-entry paths get their separators rewritten.
const _: () = assert!(JARFS_SENTINEL == cratonvm_native_io::nio_native::VFS_SENTINEL);

/// `create_string` for a path that is about to be stored in an exception, with
/// [`p57_exception_path`] applied. Every `java.nio.file` exception builder goes
/// through this rather than converting at its own call site, so a new builder
/// cannot reintroduce the divergence by forgetting.
///
/// Uninterned (round 15 wave 3, lane jdkonly2): every distinct failing path
/// used to be inserted into the never-pruned string pool, and HotSpot's `file`
/// is `Path.toString()`'s fresh string, not a literal.
pub fn p57_exception_path_string(ctx: &mut dyn NativeContext, path: &str) -> ObjectRef {
    let rendered = p57_exception_path(path);
    ctx.create_string_uninterned(rendered.as_ref())
}

/// `ERROR_DIR_NOT_EMPTY`, Windows' ENOTEMPTY. Rust reports it as an
/// uncategorised `io::Error`, so the raw OS code is the only discriminator.
#[cfg(windows)]
pub const WIN_ERROR_DIR_NOT_EMPTY: i32 = 145;

/// Map an `io::Error` from a link syscall onto the exception type the JDK
/// raises for it. `link` is the path being created/read (the `file` slot);
/// `other` is the second path a two-path operation names, if any.
pub fn p57_link_io_error(
    ctx: &mut dyn NativeContext,
    e: &std::io::Error,
    link: &str,
    other: Option<&str>,
) -> Result<MethodCallFailed, MethodCallFailed> {
    match e.kind() {
        std::io::ErrorKind::AlreadyExists => Ok(p57_file_already_exists(ctx, link)),
        std::io::ErrorKind::NotFound => p57_no_such_file(ctx, link),
        std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, link),
        _ if is_directory_not_empty(e) => p57_directory_not_empty(ctx, link),
        _ => {
            // The reason is whatever the OS said, not a string of ours.
            //
            // Windows' `ERROR_PRIVILEGE_NOT_HELD` (1314) — what
            // `CreateSymbolicLinkW` returns without `SeCreateSymbolicLinkPrivilege`
            // or Developer Mode — used to be special-cased to the hardcoded
            // English "A required privilege is not held by the client". That is
            // exactly what the JDK does NOT do: it formats the message through
            // the Win32 error table, so on a non-English host the text arrives
            // localized. Hardcoding English made CratonVM's message differ from
            // HotSpot's on precisely the host where this error is common. Rust's
            // `io::Error` Display already goes through `FormatMessage`, so
            // deleting the special case is what makes the two agree.
            //
            // Strip Rust's trailing " (os error N)" so the reason reads like the
            // JDK's, which carries only the system message text.
            let text = e.to_string();
            let reason = text
                .split(" (os error ")
                .next()
                .unwrap_or(&text)
                .to_string();
            p57_filesystem_exception(ctx, link, other, &reason)
        }
    }
}

/// Create a real symbolic link at `link` pointing at `target`.
///
/// `target` is stored verbatim — a relative target must stay relative, because
/// that is what `readSymbolicLink` has to hand back and what makes a Kubernetes
/// ConfigMap tree (`..data/<key>` plus one relative link per key) resolve.
pub fn p57_create_symbolic_link(
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
        // Only the LINK is named. Both JDK providers funnel a failed
        // `createSymbolicLink` through `rethrowAsIOException(link)` — one path —
        // so a CratonVM message reading `"<link> -> <target>: <reason>"` where
        // HotSpot reads `"<link>: <reason>"` is a divergence any test comparing
        // the message notices. `createLink` below is the genuine two-path case
        // (`rethrowAsIOException(link, existing)`) and keeps both.
        Err(e) => Err(p57_link_io_error(ctx, &e, link, None)?),
    }
}

/// Create a hard link at `link` referring to the same file as `existing`.
pub fn p57_create_hard_link(
    ctx: &mut dyn NativeContext,
    link: &str,
    existing: &str,
) -> Result<(), MethodCallFailed> {
    match std::fs::hard_link(existing, link) {
        Ok(()) => Ok(()),
        // `hard_link` reports a missing SOURCE as NotFound too, but the JDK
        // names the link being created in either case, so the generic mapping
        // is right.
        Err(e) => Err(p57_link_io_error(ctx, &e, link, Some(existing))?),
    }
}

/// Read the target of the symbolic link at `path`, as a fresh `Path`.
///
/// Handles the runtime-image (jrt) package links first — those are synthesised
/// out of the jimage, not the host filesystem — then falls through to a real
/// `readlink(2)`/`DeviceIoControl` read.
pub fn p57_read_symbolic_link(ctx: &mut dyn NativeContext, path: &str) -> MethodCallResult {
    if let Some((java_home, entry)) = jrtfs_decode(path) {
        if let Some(target) = entry.strip_prefix("packages/").and_then(|rest| {
            jrt_image(&java_home).and_then(|image| jrt_package_link_target(&image, rest))
        }) {
            let p = p57_alloc_path(ctx, &jrtfs_encode(&java_home, &format!("/{target}")));
            return Ok(Some(Value::Object(Some(p?))));
        }
        return Err(p57_no_such_file(ctx, path)?);
    }
    // Distinguish "missing" from "present but not a link" before reading, so
    // both map to the JDK's types (NoSuchFileException vs NotLinkException)
    // rather than to whatever errno `readlink` happens to produce for each.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_symlink() => {
            // The TYPE is decided here, but the REASON has to come from the OS,
            // as it does on HotSpot: `readSymbolicLink` on a regular file
            // reports `NotLinkException` carrying the platform's own text
            // ("The file or directory is not a reparse point." on Windows,
            // "Invalid argument" for EINVAL on Unix). Making the call is what
            // produces that text; inventing one here would be a string of ours
            // dressed up as the system's. If the call unexpectedly succeeds the
            // reason is simply absent, which is the old behaviour.
            let reason = std::fs::read_link(path).err().map(|e| {
                let text = e.to_string();
                text.split(" (os error ")
                    .next()
                    .unwrap_or(&text)
                    .to_string()
            });
            return Err(p57_not_link(ctx, path, reason.as_deref())?);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(p57_no_such_file(ctx, path)?)
        }
        _ => {}
    }
    match std::fs::read_link(path) {
        Ok(target) => {
            let target = target.to_string_lossy().into_owned();
            Ok(Some(Value::Object(Some(p57_alloc_path(ctx, &target)?))))
        }
        Err(e) => Err(p57_link_io_error(ctx, &e, path, None)?),
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
pub const JARFS_SENTINEL: char = '\u{1}';

/// Encoded-virtual-filesystem tags. Both the jar-FS and the runtime-image
/// (jrt) FS encode their `Path`s as sentinel-delimited strings of the form
/// `\x01<TAG>\x01<container>\x01<entry>` so the file-IO natives can detect
/// them and read the entry out of the archive / jimage instead of the host
/// filesystem. The TAG distinguishes the backing store: `JARFS` →
/// `<container>` is the jar's OS path (read via the `zip` crate); `JRTFS` →
/// `<container>` is `java.home` and `<entry>` is a jrt logical path like
/// `modules/java.base/java/lang` (read via the jimage `lib/modules`).
pub const JARFS_TAG: &str = "JARFS";

pub const JRTFS_TAG: &str = "JRTFS";

/// Generic encoder for the sentinel-delimited virtual-FS path form.
pub fn vfs_encode(tag: &str, container: &str, entry: &str) -> String {
    let entry = entry.trim_start_matches('/');
    format!("{JARFS_SENTINEL}{tag}{JARFS_SENTINEL}{container}{JARFS_SENTINEL}{entry}")
}

/// Generic decoder: returns `(tag, container, entry)` for any recognised
/// virtual-FS path, or `None` for a plain host path. The tag is mapped to one
/// of the `&'static` tag constants so callers can compare it by identity/value.
pub fn vfs_decode(p: &str) -> Option<(&'static str, String, String)> {
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
    // `\` back to `/`: the string this decodes may be one a Path's own Java
    // field stored on Windows (`p57_write_path_fields`' `stored` computation
    // converts `/` to `\` there — never here — so real
    // `WindowsPath.getFileName()`'s raw `lastIndexOf('\\')` scan lands on a
    // boundary instead of finding none and returning the whole sentinel
    // string; see that function's own doc). Every consumer of this decoder
    // wants the VM-internal `/` form regardless of which spelling the string
    // it was handed came from, so the un-conversion lives HERE, once, rather
    // than at each of this function's dozen-plus callers.
    let (container, entry) = if cfg!(windows) {
        (
            rest[..c_end].replace('\\', "/"),
            rest[c_end + JARFS_SENTINEL.len_utf8()..].replace('\\', "/"),
        )
    } else {
        (
            rest[..c_end].to_string(),
            rest[c_end + JARFS_SENTINEL.len_utf8()..].to_string(),
        )
    };
    let tag: &'static str = match tag {
        t if t == JARFS_TAG => JARFS_TAG,
        t if t == JRTFS_TAG => JRTFS_TAG,
        _ => return None,
    };
    Some((tag, container, entry))}

pub fn jarfs_encode(jar: &str, entry: &str) -> String {
    vfs_encode(JARFS_TAG, jar, entry)
}

/// If `p` is a jar-FS encoded path, return `(jar_os_path, entry)`.
pub fn jarfs_decode(p: &str) -> Option<(String, String)> {
    match vfs_decode(p) {
        Some((tag, c, e)) if tag == JARFS_TAG => Some((c, e)),
        _ => None,
    }
}

pub fn jrtfs_encode(java_home: &str, entry: &str) -> String {
    vfs_encode(JRTFS_TAG, java_home, entry)
}

/// If `p` is a jrt-FS (runtime-image) encoded path, return `(java_home, entry)`
/// where `entry` is the jrt logical path without a leading slash (e.g.
/// `modules/java.base/java/lang`).
pub fn jrtfs_decode(p: &str) -> Option<(String, String)> {
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
pub fn jar_bytes_cached(jar: &str) -> Option<std::sync::Arc<Vec<u8>>> {
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
pub struct JarFsIndex {
    pub names: Vec<String>,
    pub sizes: std::collections::HashMap<String, u64>,
    pub raw_names: std::collections::HashMap<String, String>,
    /// Immediate children keyed by their parent directory.  Javac's archive
    /// indexer walks every package directory; deriving children by scanning a
    /// prefix range for each directory makes that first walk quadratic for
    /// deeply packaged archives.
    pub children: std::collections::HashMap<String, Vec<(String, bool)>>,
}

/// Keyed on (path, [`crate::net_phase_e::archive_stamp`]) — see
/// `jar_bytes_cached` for why a path-only key is wrong for a redeployable
/// archive.
pub fn jar_index(jar: &str) -> Option<std::sync::Arc<JarFsIndex>> {
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
            let raw = f.name().ok()?.into_owned();
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

pub fn jarfs_read_entry(jar: &str, entry: &str) -> std::io::Result<Vec<u8>> {
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

pub fn jarfs_entry_size(jar: &str, entry: &str) -> std::io::Result<i64> {
    let entry = entry.trim_start_matches('/');
    let index = jar_index(jar).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    index
        .sizes
        .get(entry)
        .map(|size| *size as i64)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}

pub fn jarfs_rewrite_entry(
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
                    let name = f
                        .name()
                        .map_err(|e| {
                            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                        })?
                        .into_owned();
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

pub fn jarfs_create_dir_entry(jar: &str, entry: &str) -> std::io::Result<()> {
    jarfs_rewrite_entry(jar, entry, None)
}

pub fn jarfs_write_file_entry(jar: &str, entry: &str, bytes: &[u8]) -> std::io::Result<()> {
    jarfs_rewrite_entry(jar, entry, Some(bytes))
}

/// Kind of a jar-FS path: regular file, directory, or absent.
pub enum JarFsKind {
    File,
    Dir,
    Absent,
}

pub fn jarfs_classify(jar: &str, entry: &str) -> JarFsKind {
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
pub fn jarfs_list_dir(jar: &str, dir: &str) -> Vec<String> {
    jarfs_list_dir_classified(jar, dir)
        .into_iter()
        .map(|(child, _)| child)
        .collect()
}

/// List immediate children of `dir` inside a JAR together with an
/// is-directory flag from the cached direct-child index.  This keeps a full
/// archive walk linear in its entries rather than repeatedly scanning every
/// descendant below each package directory.
pub fn jarfs_list_dir_classified(jar: &str, dir: &str) -> Vec<(String, bool)> {
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
pub struct JrtImage {
    pub reader: cratonvm_reader::JImageReader,
    pub entries: Vec<String>,
    pub package_modules: std::collections::BTreeMap<String, Vec<String>>,
    pub entry_sizes: std::collections::HashMap<String, u64>,
}

pub fn jrt_image(java_home: &str) -> Option<std::sync::Arc<JrtImage>> {
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

/// Every resource name inside `module`, exactly what real
/// `jdk.internal.module.SystemModuleFinders$SystemModuleReader.list()`
/// returns for it: paths relative to the module root, `/`-separated, no
/// leading `/` (`"java/lang/Object.class"`, `"module-info.class"`, ...).
///
/// Backs `native_system_module_reader_list` (`jboss_jdkspecific.rs`), which
/// used to answer unconditionally empty — a deliberate stub, correct as long
/// as nothing walked a system module's OWN resource listing. In-process
/// `javac`'s platform-classpath package scan does exactly that
/// (`Locations$SystemModulesLocationHandler`, reached through
/// `StandardJavaFileManager.list(PLATFORM_CLASS_PATH, "java.lang", ...)`),
/// and an empty list here is indistinguishable from "java.lang doesn't
/// exist" — `compiler.err.no.java.lang`, "Unable to find package java.lang
/// in platform classes". MEASURED: `TestCompilerTests`/
/// `CandidateComponentsIndexerTests` under `--real-jdk`; `ModuleReader.find`
/// (real bytecode, unaffected by the stub) already resolved individual
/// resources correctly, which is why raw `Files`/`ModuleFinder` probes on the
/// same jrt image did not reproduce this — only `.list()`'s callers did.
pub fn jrt_module_resource_list(java_home: &str, module: &str) -> Vec<String> {
    let Some(img) = jrt_image(java_home) else {
        return Vec::new();
    };
    let prefix = format!("/{module}/");
    img.entries
        .iter()
        .filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
        .collect()
}

/// Map a jrt logical entry (`modules/java.base/java/lang/String.class`) to the
/// jimage resource path (`/java.base/java/lang/String.class`). Returns `None`
/// for the synthetic container dirs (`""`, `modules`) which have no jimage
/// resource of their own.
pub fn jrt_entry_to_image(entry: &str) -> Option<String> {
    let e = entry.trim_matches('/');
    let rest = e.strip_prefix("modules")?;
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        None
    } else {
        Some(format!("/{rest}"))
    }
}

pub fn jrt_img_is_file(img: &JrtImage, path: &str) -> bool {
    img.entries
        .binary_search_by(|e| e.as_str().cmp(path))
        .is_ok()
}

pub fn jrt_img_is_dir(img: &JrtImage, path: &str) -> bool {
    let prefix = format!("{path}/");
    let idx = img
        .entries
        .partition_point(|e| e.as_str() < prefix.as_str());
    img.entries.get(idx).is_some_and(|e| e.starts_with(&prefix))
}

pub fn jrt_package_link_target(img: &JrtImage, rest: &str) -> Option<String> {
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

pub fn jrt_package_backing_entry(img: &JrtImage, rest: &str) -> Option<String> {
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

pub fn jrt_package_path_kind(img: &JrtImage, rest: &str) -> JarFsKind {
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

pub fn jrtfs_classify(java_home: &str, entry: &str) -> JarFsKind {
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
pub fn jrtfs_list_dir_classified(java_home: &str, entry: &str) -> Vec<(String, bool)> {
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
            seen.insert(module.to_string());        }
        if false {
            eprintln!(
                "[jrt-modules-dbg] img.entries.len()={} distinct_modules={} contains_java_base={} sample_java_base_entries={:?}",
                img.entries.len(),
                seen.len(),
                seen.contains("java.base"),
                img.entries.iter().filter(|p| p.starts_with("/java.base/")).take(3).collect::<Vec<_>>()
            );
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
pub fn jrtfs_list_class_binary_names(
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
            classes.push(class_path.replace('/', "."));        }    }

    classes.sort_unstable();
    classes
}



pub fn jrtfs_entry_size(java_home: &str, entry: &str) -> std::io::Result<i64> {
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
pub fn jrtfs_read(java_home: &str, entry: &str) -> std::io::Result<Vec<u8>> {
    if false {
        eprintln!("[dbg-jrt-read] {entry}");    }
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
pub fn vfs_classify(p: &str) -> Option<JarFsKind> {
    if let Some((jar, entry)) = jarfs_decode(p) {
        Some(jarfs_classify(&jar, &entry))
    } else {
        jrtfs_decode(p).map(|(jh, entry)| jrtfs_classify(&jh, &entry))
    }
}

/// Read any virtual-FS (jar / jrt) encoded entry's bytes. Returns `None` for a
/// plain host path (caller reads it via `std::fs`).
pub fn vfs_read(p: &str) -> Option<std::io::Result<Vec<u8>>> {
    if let Some((jar, entry)) = jarfs_decode(p) {
        Some(jarfs_read_entry(&jar, &entry))
    } else {
        jrtfs_decode(p).map(|(jh, entry)| jrtfs_read(&jh, &entry))
    }
}

/// Slots on the in-memory `SeekableByteChannel` [`FileSystemProvider
/// .newByteChannel`'s `vfs_read`][self] arm mints for a jar/jrt entry: 0=data
/// (`byte[]`, the whole entry, already decompressed), 1=position (`int`),
/// 2=size (`int`, `data.length` — kept as its own slot rather than read from
/// the array each time so `truncate`/a future writable arm has somewhere to
/// diverge from the backing array's length), 3=closed (`int`, 0/1).
pub const P57_VFS_CHANNEL_SLOTS: usize = 4;

pub fn p57_vfs_channel_closed(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    ctx.get_field(this, 3).as_int().unwrap_or(0) != 0
}

/// `size`/`read`/`position`/`isOpen`/`close` for the in-memory channel
/// `FileSystemProvider.newByteChannel` mints for a jar/jrt entry.
///
/// Minted AS the bare `java/nio/channels/SeekableByteChannel` INTERFACE
/// (JVMS 6.5's `new` on an interface can't happen from real bytecode, but a
/// native allocator is not bytecode) — deliberately, so nothing here needs a
/// candidate-class walk the way [`FILE_SYSTEM_PROVIDER_IMPLS`] does: there is
/// no real `sun.nio.*` class that reads three private ints off a foreign
/// `byte[]` the way this carrier does, so a "concrete" stand-in would be
/// exactly as synthetic while pretending otherwise. What matters is that
/// `SeekableByteChannel` itself declares NO method with a `Code` attribute —
/// every method here is abstract on the interface — so real dispatch can
/// never shadow these registrations the way a concrete
/// `sun.nio.fs.*FileSystemProvider`'s own `getScheme()` shadowed
/// [`p57_alloc_provider`]'s appended slot. Before this registrar existed, the
/// SAME allocation shipped with no native backing at all: any real method
/// call on it — `size()` first, since every reader checks that before
/// reading — was `AbstractMethodError: method
/// java/nio/channels/SeekableByteChannel.size()J has no Code attribute`. That
/// went unnoticed until jar filesystems could be mounted at all
/// (`p57_alloc_provider`'s "jar" scheme fix, this same commit's sibling), and
/// then it broke every `Files.newByteChannel`/`Files.readAllBytes` read of a
/// mounted jar-fs entry — `NestedFileSystemZipFileSystemIntegrationTests`
/// among them.
///
/// Read-only, matching `newByteChannel`'s own comment above
/// ("read-only via the methodless stub — unchanged behaviour"): `write` and
/// `truncate` are left unregistered, so those two still `AbstractMethodError`
/// rather than silently accepting a write nothing persists. A future writable
/// arm would need its own slot for "dirty", not just method bodies.
pub fn register_vfs_seekable_byte_channel_natives(r: &mut NativeMethodRegistry) {
    let sbc = "java/nio/channels/SeekableByteChannel";

    r.register(sbc, "size", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if p57_vfs_channel_closed(ctx, this) {
            return Err(p57_closed_channel(ctx));
        }
        let size = ctx.get_field(this, 2).as_int().unwrap_or(0);
        Ok(Some(Value::Long(size as i64)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(sbc, "position", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if p57_vfs_channel_closed(ctx, this) {
            return Err(p57_closed_channel(ctx));
        }
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        Ok(Some(Value::Long(pos as i64)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(
        sbc,
        "position",
        "(J)Ljava/nio/channels/SeekableByteChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if p57_vfs_channel_closed(ctx, this) {
                return Err(p57_closed_channel(ctx));
            }
            let new_pos = args.get(1).and_then(Value::as_long).unwrap_or(0);
            // Real `SeekableByteChannel.position(long)` rejects a negative
            // position; a position past `size` is legal (matches HotSpot —
            // the next `read` just answers EOF).
            if new_pos < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("Negative position: {new_pos}"),
                }
                .into());
            }
            ctx.set_field(this, 1, Value::Int(new_pos as i32));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    r.register(sbc, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(!p57_vfs_channel_closed(
            ctx, this,
        )))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(sbc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 3, Value::Int(1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(sbc, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if p57_vfs_channel_closed(ctx, this) {
            return Err(p57_closed_channel(ctx));
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("SeekableByteChannel.read: null destination buffer".into()),
                }
                .into())
            }
        };
        let data_arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let size = ctx.get_field(this, 2).as_int().unwrap_or(0) as usize;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        if pos >= size {
            return Ok(Some(Value::Int(-1)));
        }
        // ByteBuffer: field 0 = backing array, 1 = position, 2 = limit — the
        // same layout `p57_fc_read` reads.
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let want = bb_lim.saturating_sub(bb_pos).min(size - pos);
        if want == 0 {
            return Ok(Some(Value::Int(0)));
        }
        if let Value::Object(Some(dst)) = ctx.get_field(bb, 0) {
            for i in 0..want {
                let b = ctx.get_array_element(data_arr, pos + i);
                ctx.set_array_element(dst, bb_pos + i, b);
            }
        }
        ctx.set_field(bb, 1, Value::Int((bb_pos + want) as i32));
        ctx.set_field(this, 1, Value::Int((pos + want) as i32));
        Ok(Some(Value::Int(want as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

/// Immediate children of a directory `p` (host / jar / jrt), as encoded path
/// strings ready for `p57_alloc_path`.
pub fn vfs_or_host_list(p: &str) -> Vec<String> {
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
pub fn vfs_or_host_is_dir(p: &str) -> bool {
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
pub fn vfs_or_host_is_walkable_dir(p: &str, follow_links: bool) -> bool {
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
pub fn vfs_or_host_walk(
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
pub fn p57_link_options_nofollow(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> bool {
    matches!(arg, Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0)
}

/// `Files.exists(Path, LinkOption...)`. Shared by the two registrations of this
/// triple (`register_phase57_nio_file` and `register_p61_files_path`) so the
/// pair cannot drift — they already had, and the NOFOLLOW_LINKS repair landed
/// on the losing copy first.
pub fn p57_files_exists_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    let Some(Value::Object(Some(path_ref))) = args.first() else {
        return false;
    };
    // The LinkOption array is asked BEFORE the Path read, which can run Java
    // (a foreign Path's `toString()`): `args` is a snapshot no collection
    // rewrites (gc-common w21-b).
    let nofollow = p57_link_options_nofollow(ctx, args.get(1));
    let mut path = *path_ref;
    let path_str = p57_read_path(ctx, &mut path);
    match vfs_classify(&path_str) {
        Some(kind) => !matches!(kind, JarFsKind::Absent),
        // NOFOLLOW_LINKS asks about the LINK, so a dangling symbolic link
        // exists. `Path::exists()` resolves the link and answered false.
        None if nofollow => std::fs::symlink_metadata(&path_str).is_ok(),
        None => std::path::Path::new(&path_str).exists(),
    }
}

/// `Files.isDirectory(Path, LinkOption...)`. Shared for the same reason as
/// [`p57_files_exists_impl`].
pub fn p57_files_is_directory_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    let Some(Value::Object(Some(path_ref))) = args.first() else {
        return false;
    };
    // The LinkOption array is asked BEFORE the Path read, which can run Java
    // (a foreign Path's `toString()`): `args` is a snapshot no collection
    // rewrites (gc-common w21-b).
    let nofollow = p57_link_options_nofollow(ctx, args.get(1));
    let mut path = *path_ref;
    let path_str = p57_read_path(ctx, &mut path);
    match vfs_classify(&path_str) {
        Some(kind) => matches!(kind, JarFsKind::Dir),
        // Under NOFOLLOW_LINKS a link to a directory is a LINK, not a directory.
        None if nofollow => std::fs::symlink_metadata(&path_str).is_ok_and(|m| m.is_dir()),
        None => std::path::Path::new(&path_str).is_dir(),
    }
}

/// `Files.isRegularFile(Path, LinkOption...)`. Shared for the same reason as
/// [`p57_files_exists_impl`].
pub fn p57_files_is_regular_file_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    let Some(Value::Object(Some(path_ref))) = args.first() else {
        return false;
    };
    // The LinkOption array is asked BEFORE the Path read, which can run Java
    // (a foreign Path's `toString()`): `args` is a snapshot no collection
    // rewrites (gc-common w21-b).
    let nofollow = p57_link_options_nofollow(ctx, args.get(1));
    let mut path = *path_ref;
    let path_str = p57_read_path(ctx, &mut path);
    match vfs_classify(&path_str) {
        Some(kind) => matches!(kind, JarFsKind::File),
        None if nofollow => std::fs::symlink_metadata(&path_str).is_ok_and(|m| m.is_file()),
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
pub fn p57_visit_options_follow_links(
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
pub fn p57_uri_full_text(ctx: &mut dyn NativeContext, uri: ObjectRef) -> String {
    // `--real-jdk`: every `URI` is a real one there (SBR wave 1 retired the
    // `java/net/URI` bridges and its producers run the JDK parser), so its
    // text is `string`, or -- for one `resolve`/`normalize` assembled -- the
    // JDK's own `defineString` over its fields. The slot scan below would pick
    // the raw `path` slot of such a URI, a scheme-less fragment of it.
    // nio21 wave 4 L1: the same holds under `--compatible` with the `jdk-uri`
    // row (now default ON), where every producer adopts the JDK parser's
    // fields too (`net_phase_e::jdk_uri_model`); before this gate followed the
    // row, `Path.of(base.resolve("x"))` read the raw slots of a URI whose
    // `string` cache is still null (probe `Nio21W3Doors uri resolved-path`).
    if crate::net_phase_e::jdk_uri_model(&*ctx) {
        let t = crate::net_phase_e::uri_raw_string(ctx, uri);
        if !t.is_empty() {
            return t;
        }
    }
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
pub fn p57_uri_is_opaque_file(text: &str) -> bool {
    text.strip_prefix("file:")
        .is_some_and(|rest| !rest.starts_with('/'))
}

/// Convert `jar:file:/C:/x.jar!/entry` / `file:///C:/x.jar` URI text to the
/// OS path of the backing JAR file. Returns `None` for text that does not
/// look like a file-backed URI (empty / unparseable).
pub fn p57_jar_uri_to_os_path(text: &str) -> Option<String> {
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
pub fn p57_jar_uri_to_entry_path(text: &str) -> Option<(String, String)> {
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
pub fn p57_io_error(e: &std::io::Error) -> cratonvm_types::error::MethodCallFailed {
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
///
/// Callers MUST pass `lang_system::saved_system_property(ctx, "java.io.tmpdir")`,
/// not `ctx.get_system_property("java.io.tmpdir")`: the latter is the live
/// store, and HotSpot's own `File$TempDirectory` reads a boot-time snapshot
/// that a later `System.setProperty` cannot move. See
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub fn jdk_temp_dir(tmpdir_prop: Option<String>) -> std::path::PathBuf {
    match tmpdir_prop {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => std::path::PathBuf::from(cratonvm_native_api::os_tmpdir::java_io_tmpdir()),
    }
}

/// Create a fresh, empty temp file exclusively (`O_CREAT|O_EXCL`) inside
/// `dir`, returning its full path. Failures MUST surface as `IOException` —
/// that is the `File.createTempFile`/`Files.createTempFile` contract, and
/// callers rely on the exception for fallback logic (mockk's `BootJarLoader`
/// falls back to a CWD jar when the temp dir is unusable; the old
/// `let _ = std::fs::File::create(..)` swallowed the error and the divergence
/// only surfaced much later as an unrelated-looking failure).
pub fn jdk_create_temp_file(
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
pub fn p57_read_to_string(p: &str) -> std::io::Result<String> {
    let bytes = match vfs_read(p) {
        Some(r) => r?,
        None => std::fs::read(p)?,
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())}

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
pub fn p57_normalize_path(path: &str) -> String {
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
pub fn p57_relativize(base: &str, target: &str) -> Option<String> {
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

