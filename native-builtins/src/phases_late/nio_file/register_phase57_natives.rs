#[allow(unused_imports)]
use super::*;


// ============================================================================
// Phase 57: NIO File extras (Path/Paths/Files), ProcessBuilder/Process,
//           java.text (DecimalFormat, SimpleDateFormat, MessageFormat)
// ============================================================================

pub(crate) fn register_phase57_natives(registry: &mut NativeMethodRegistry) {
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
}


/// Write `text` into a freshly allocated Path's slots — **the one writer**, so
/// no producer of this carrier can drift from the map the readers use.
///
/// Both slots publish together or not at all. `stringValue` alone would leave
/// the real `compareTo` (so `equals`), `hashCode` and `initOffsets` — which
/// `getFileName`/`getParent`/`getNameCount` all go through — reading a null
/// `byte[]`, which trades a wrong answer for an NPE: worse than either
/// endpoint, and the lesson `install_real_stream_fields` paid for on
/// `System.out`.
///
/// `stringValue` is written as well as the bytes, so real `toString()` returns
/// it directly and never has to decode them back through `fs`.
///
/// The encoding is the JDK's own: `UnixPath` stores `Util.toBytes(input)`,
/// which on any modern JDK is the platform charset's encoding of the path
/// string — UTF-8 on every host this VM supports.
///
/// `fs` is resolved from `text` itself: a jar/jrt-encoded string (see
/// `jarfs_decode`/`jrtfs_decode`) gets the owning virtual FileSystem, anything
/// else gets the default-filesystem singleton
/// (`p57_default_filesystem_singleton`). Until 2026-09-16 this slot was simply
/// never written — invisible as long as no real bytecode read it, but real
/// `WindowsFileSystemProvider`/`UnixFileSystemProvider` methods
/// (`createLink`/`createSymbolicLink`/`newFileChannel`/`readSymbolicLink`, see
/// lane-4 §9.28) call `Path.getFileSystem()` internally and NPE on a null
/// slot. The jar/jrt case mints a fresh FileSystem per path (no cross-path
/// identity), matching every other jar/jrt FS allocation site in this file —
/// none of them cache by container either.
///
/// Returns the (possibly relocated) object: each allocation below is a GC
/// point, so the caller must use the returned reference and not the one it
/// passed in.
pub fn p57_write_path_fields(
    ctx: &mut dyn NativeContext,
    path_obj: ObjectRef,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let slots = p57_path_slots_init(ctx);
    let pin = ctx.pin_native_root(path_obj);
    // Classify FIRST (Windows real class only; a jar/jrt sentinel string is not
    // a Windows path). The parser's own `path()` is what a real `WindowsPath`
    // stores — separator runs collapsed, a UNC root closed with `\` — so the
    // slot is filled from it rather than from `text` (wave 15): a `path` this
    // VM stored uncollapsed made `a//b` unequal to its parsed twin and gave
    // `hashCode`/`equals`/`relativize` a different answer than HotSpot's.
    let is_vfs = jarfs_decode(text).is_some() || jrtfs_decode(text).is_some();
    let classified: Option<(ObjectRef, usize, String, Option<String>)> =
        if slots.kind.is_some() && slots.root.is_some() {
            if is_vfs {
                // A jar/jrt sentinel string is not a Windows path at all, so
                // `WindowsPathParser` never runs against it — but the fields
                // it would have written are NOT skippable: real
                // `WindowsPath.root` is never `null` (a rootless path gets
                // `""`), and in-process `javac`'s own module-path scanning
                // (`Locations$SystemModulesLocationHandler.initSystemModules`,
                // listing the runtime image / a classpath jar) calls
                // `getFileName`/`isSameDrive`/etc. on exactly these entries.
                // Measured: `TestCompilerTests`/`CandidateComponentsIndexerTests`
                // under `--real-jdk` NPE inside real `WindowsPath.getFileName`
                // ("this.root is null") the moment a jrt-mounted directory
                // listing is walked. Same `RELATIVE`/`""` fallback the
                // unparseable-real-path branch below already uses.
                p57_windows_path_relative_fallback(ctx)
                    .map(|k| (k, ctx.pin_native_root(k), String::new(), None))
            } else {
                // §9.36: `p57_classify_windows_path` returns `None` whenever the
                // REAL `WindowsPathParser.parse` throws (`"//"` is the measured
                // case). Real `WindowsPath.root` is never `null` — a rootless path
                // gets `""` — so a `RELATIVE`/`""` fallback keeps real bytecode
                // from an NPE on a path this VM's looser acceptance let through.
                match p57_classify_windows_path(ctx, text) {
                    Some((k, root, norm)) => Some((k, ctx.pin_native_root(k), root, Some(norm))),
                    None => p57_windows_path_relative_fallback(ctx)
                        .map(|k| (k, ctx.pin_native_root(k), String::new(), None)),
                }
            }
        } else if slots.kind.is_some() && slots.root.is_some() && is_vfs {
            // A jar/jrt sentinel-encoded Path is stamped `sun/nio/fs/WindowsPath`
            // the same as every other Path this VM mints (`path_layout::impl_class`
            // has one carrier class, not one per virtual filesystem), so real
            // `WindowsPath` bytecode runs against it too. `WindowsPath.resolve`/
            // `compareTo`/`equals` all read `this.type`/`other.type` directly —
            // `resolve` unconditionally, to decide whether `other` is absolute —
            // and NPE on the null this VM otherwise leaves there (this class's
            // own doc, two paragraphs up: "left unclassified — same
            // null-until-written state as before this fix"). That state was
            // survivable only as long as no real bytecode read it; once
            // `--real-jdk` made real `WindowsPath` methods the default dispatch
            // target, `PathMatchingResourcePatternResolver`'s directory walk
            // over a newly-mounted jar filesystem hit it on every
            // `Path.resolve()` (`PemContentTests` et al., loading PEM/JKS
            // content that ships inside a jar on the test classpath).
            // `RELATIVE`/`""` is the same harmless fallback the real-parse-failed
            // arm above uses — nothing compares a jar/jrt Path's `type` against
            // an actual Windows root, so it only has to be non-null, not correct.
            p57_windows_path_relative_fallback(ctx)
                .map(|k| (k, ctx.pin_native_root(k), String::new(), None))
        } else {
            None
        };
    // What the SLOTS hold is the JDK's own form (`\` on Windows); everything
    // else classifies `text`, the VM-internal `/` form.
    let stored_src: &str = classified
        .as_ref()
        .and_then(|c| c.3.as_deref())
        .unwrap_or(text);
    // Real `WindowsPath.getFileName()`/`getParent()` do not go through the
    // `offsets` cache at all (confirmed via `javap`: both scan `this.path`
    // directly for a literal `'\\'` via `String.lastIndexOf(92)`, completely
    // independent of `offsets` — see `retired_shadow.rs`'s
    // `RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES` doc, which retired exactly
    // this pair on exactly that finding for the GENUINE-path case). A jar/jrt
    // sentinel string was left in the VM-internal `/` form here (`vfs_decode`
    // needs SOME encoding this VM controls end to end), so that raw scan
    // always finds zero `\` and returns the entire sentinel-and-all string as
    // "the file name" — MEASURED via `ModuleReader.list()`'s own entries
    // (2026-09-22): `javac`'s `Locations$SystemModulesLocationHandler`
    // extracts a module name via exactly this call over a
    // `Files.newDirectoryStream(jrtRoot)` listing. Converting `/` to `\` in
    // the JAVA-VISIBLE string only (not in any Rust-side `text`/`entry`
    // already captured above) makes that same real scan land on the right
    // boundary, matching HotSpot's own algorithm instead of reimplementing
    // it. `vfs_decode` undoes this — `\` back to `/` — on every string it
    // reads back from a Path's field, so nothing downstream of it (every
    // consumer goes through `jarfs_decode`/`jrtfs_decode`) sees a change.
    let stored: String = if is_vfs && cfg!(windows) {
        stored_src.replace('/', "\\")
    } else {
        cratonvm_native_api::path_layout::stored_form(stored_src).into_owned()
    };
    let s = ctx.create_string(&stored);
    let obj = ctx.read_native_pin(pin, path_obj);
    ctx.set_field(obj, slots.string, Value::Object(Some(s)));
    if let Some(bytes_slot) = slots.bytes {
        let raw = stored.as_bytes();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, raw.len());
        ctx.write_byte_array_from(arr, 0, raw);
        let obj = ctx.read_native_pin(pin, path_obj);
        ctx.set_field(obj, bytes_slot, Value::Object(Some(arr)));
    }
    let fs = if let Some((jar, _entry)) = jarfs_decode(text) {
        p57_alloc_jar_filesystem(ctx, &jar)
    } else if let Some((java_home, _entry)) = jrtfs_decode(text) {
        p57_alloc_jrt_filesystem(ctx, &java_home)
    } else {
        p57_default_filesystem_singleton(ctx)
    };
    let fs = match fs {
        Ok(fs) => fs,
        Err(e) => {
            ctx.unpin_native_roots(pin);
            return Err(e);
        }
    };
    let obj = ctx.read_native_pin(pin, path_obj);
    ctx.set_field(obj, slots.fs, Value::Object(Some(fs)));

    // `type`/`root` — see lane-4 doc §9.30's fourth-defect account. Present
    // only once the carrier resolves to genuine `WindowsPath` (`slots.kind`/
    // `slots.root` both `Some`). A jar/jrt sentinel-encoded string is not a
    // Windows path at all (see `jarfs_decode`/`jrtfs_decode`), but the carrier
    // class IS `WindowsPath` regardless — `path_layout::impl_class` stamps
    // every Path this VM mints with the one class, not one per virtual
    // filesystem — so `classified` above still fills these with the harmless
    // `RELATIVE`/`""` fallback rather than leaving them null: see that branch's
    // own doc for the real-bytecode NPE this closes.
    if let (Some(kind_slot), Some(root_slot)) = (slots.kind, slots.root) {
        {
            if let Some((kind_ref0, kind_pin, root_str, norm)) = classified {
                let genuinely_classified = norm;
                let root_obj = ctx.create_string(&root_str);
                let kind_ref = ctx.read_native_pin(kind_pin, kind_ref0);
                let obj = ctx.read_native_pin(pin, path_obj);
                ctx.set_field(obj, kind_slot, Value::Object(Some(kind_ref)));
                let obj = ctx.read_native_pin(pin, path_obj);
                ctx.set_field(obj, root_slot, Value::Object(Some(root_obj)));

                // §9.36 second follow-up: pre-populate `offsets` too, so real
                // `WindowsPath.initOffsets()` — a null-guarded lazy cache —
                // short-circuits to this answer. (Wave 15: `path` is now
                // stored `\`, the JDK's own form, so this is a cache of what
                // `initOffsets` would compute anyway, not a workaround for a
                // `/`-stored string it could not scan.) The one difference
                // that survives is separator RUNS — see
                // `p57_windows_path_offsets`'s own doc.
                //
                // §9.36 third follow-up: when classification itself failed
                // (`genuinely_classified` is `None` — real
                // `WindowsPathParser` would have thrown, e.g. for `"//"`),
                // do NOT run the real scan over `text` at all. There is no
                // correct offsets answer for an input HotSpot never
                // constructs a `WindowsPath` from in the first place, and
                // naively splitting on every `/` produced a nonsensical
                // multi-component shape for a path that is nothing BUT
                // separators (`"//"` scanned as two one-character "names"),
                // which is what crashed `normalize()` next. Zero components
                // is the same, already-proven-safe shape a legitimate
                // root-only path (`path[/]`) already uses successfully —
                // "no name components" is a case every real accessor here
                // already handles, unlike "//"'s own invented split.
                if let Some(offsets_slot) = slots.offsets {
                    use cratonvm_types::ArrayElementType;
                    let offsets = if is_vfs {
                        // `stored` is now `\`-delimited for the vfs case too
                        // (see this function's `stored` computation above),
                        // so the same real-`WindowsPath`-shaped scan applies;
                        // root is always `""` for a vfs path.
                        p57_windows_path_offsets(&stored, "")
                    } else if genuinely_classified.is_some() {
                        p57_windows_path_offsets(&stored, &root_str)
                    } else {
                        Vec::new()
                    };
                    let arr = ctx.new_array(ArrayElementType::Reference, offsets.len());
                    let arr_pin = ctx.pin_native_root(arr);
                    for (idx, off) in offsets.iter().enumerate() {
                        let boxed = box_int(ctx, *off);
                        let arr = ctx.read_native_pin(arr_pin, arr);
                        ctx.set_array_element(arr, idx, boxed);
                    }
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.unpin_native_roots(arr_pin);
                    let obj = ctx.read_native_pin(pin, path_obj);
                    ctx.set_field(obj, offsets_slot, Value::Object(Some(arr)));
                }
            }
        }
    }

    let obj = ctx.read_native_pin(pin, path_obj);
    ctx.unpin_native_roots(pin);
    Ok(obj)
}

/// Classify `text` as a Windows path via the real, already-fully-functional
/// `sun.nio.fs.WindowsPathParser.parse` — the same route
/// `p57_alloc_default_filesystem_concrete` uses for
/// `defaultDirectory`/`defaultRoot`, for the same reason: byte-identical to
/// what HotSpot's own `WindowsPath.parse` factory would have classified, with
/// no separator/drive/UNC-parsing reimplementation in Rust.
///
/// Returns `None` on any failure — including a thrown `InvalidPathException`
/// (a jar/jrt sentinel-encoded string would throw one, which is why the
/// caller filters those out first, but a caller-supplied string can be
/// arbitrary) — so `type`/`root` are simply left unwritten rather than
/// failing the whole path allocation over a field two producing crates
/// (`native-builtins`, `native-io`) never populated before this fix anyway.
///
/// `root`'s content is returned as the parser produced it (`\`), the same
/// form `path` is now stored in (wave 15) — real readers compare `root`
/// against `path` by length and by content (`startsWith`/`relativize`), so the
/// two must agree.
pub fn p57_classify_windows_path(
    ctx: &mut dyn NativeContext,
    text: &str,
) -> Option<(ObjectRef, String, String)> {
    if !cfg!(windows) {
        return None;
    }
    let input = ctx.create_string(text);
    let parsed = ctx.invoke(
        "sun/nio/fs/WindowsPathParser",
        "parse",
        "(Ljava/lang/String;)Lsun/nio/fs/WindowsPathParser$Result;",
        &[Value::Object(Some(input))],
    );
    let result = match parsed {
        Ok(Some(Value::Object(Some(r)))) => r,
        _ => return None,
    };
    let result_pin = ctx.pin_native_root(result);
    let kind = ctx.invoke_virtual(result, "type", "()Lsun/nio/fs/WindowsPathType;", &[]);
    let kind_ref = match kind {
        Ok(Some(Value::Object(Some(k)))) => k,
        _ => {
            ctx.unpin_native_roots(result_pin);
            return None;
        }
    };
    let kind_pin = ctx.pin_native_root(kind_ref);
    let result = ctx.read_native_pin(result_pin, result);
    let root = ctx.invoke_virtual(result, "root", "()Ljava/lang/String;", &[]);
    let root_str = match root {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let result = ctx.read_native_pin(result_pin, result);
    let normalised = ctx.invoke_virtual(result, "path", "()Ljava/lang/String;", &[]);
    let path_str = match normalised {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    let kind_ref = ctx.read_native_pin(kind_pin, kind_ref);
    ctx.unpin_native_roots(kind_pin);
    ctx.unpin_native_roots(result_pin);
    Some((
        kind_ref,
        root_str,
        path_str.unwrap_or_else(|| text.to_string()),
    ))
}

/// `sun.nio.fs.WindowsPathType.RELATIVE`, the conservative `type` fallback
/// [`p57_write_path_fields`] uses when [`p57_classify_windows_path`] cannot
/// classify `text` at all (real `WindowsPathParser.parse` threw). Fetched via
/// the enum's own compiler-generated `valueOf(String)` rather than a raw
/// static-field read, matching this file's existing preference (see
/// `p57_classify_windows_path`'s own doc) for routing through real JDK code
/// over a hand-kept second copy of its logic.
pub fn p57_windows_path_relative_fallback(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    if !cfg!(windows) {
        return None;
    }
    let name = ctx.create_string("RELATIVE");
    match ctx.invoke(
        "sun/nio/fs/WindowsPathType",
        "valueOf",
        "(Ljava/lang/String;)Lsun/nio/fs/WindowsPathType;",
        &[Value::Object(Some(name))],
    ) {
        Ok(Some(Value::Object(Some(k)))) => Some(k),
        _ => None,
    }
}

/// The name-component START offsets real `WindowsPath.initOffsets()` would
/// compute — mirrored from its own bytecode (`javap -c`: scan `path` from
/// `root.length()`, record the index a segment STARTS at each time a
/// separator is seen, then the final trailing segment if non-empty; an
/// empty `path` is the one-element case `[0]`) with two differences from a
/// literal translation, both forced by this VM's OWN storage rather than
/// a difference in what real `WindowsPath` computes:
///
/// 1. (Wave 15: no longer a difference.) The separator scanned for is `\`,
///    what `path` is stored with since `path_layout::stored_form`; before
///    wave 15 it was `/` (lane-4 doc §9.36's second follow-up).
/// 2. A run of CONSECUTIVE separators only ever closes ONE segment, never
///    an empty one in between. Real `WindowsPath.path` is always the
///    ALREADY-NORMALIZED string `WindowsPathParser.parse(..).path()`
///    returns — single separators only, by construction — so real
///    `initOffsets()`'s naive per-character scan never needs to handle a
///    double separator; it structurally cannot occur. This VM's own
///    `p57_alloc_path`, by contrast, only trims a trailing separator and
///    converts `\` to `/` — it does NOT collapse an internal `a//b` into
///    `a/b` (changing that is a much larger, VM-wide question: `path` is
///    read by this VM's own natives everywhere, not only by this fix).
///    A literal translation of `initOffsets()` over THIS VM's uncollapsed
///    storage would count the same separator run's inner boundary as its
///    own (empty) segment — measured: `"a//b"` came back `nameCount=3`,
///    not `2` (§9.36 third follow-up). Skipping an empty span keeps every
///    offset a valid index into the STORED (uncollapsed) string — real
///    accessors read `path.substring(offset, nextSeparatorOrEnd)`, which
///    is correct regardless of what sits between two offsets — while
///    producing the same LOGICAL component count real, fully-normalized
///    `WindowsPath.initOffsets()` would have, without changing what
///    `path` itself holds.
pub fn p57_windows_path_offsets(path: &str, root: &str) -> Vec<i32> {
    if path.is_empty() {
        return vec![0];
    }
    // UTF-16 code-unit indices, not UTF-8 byte offsets: `offsets` are indices
    // into a Java `String`, and a non-ASCII name (`é\è`) put every later
    // offset past the end of the string (`StringIndexOutOfBounds` in the real
    // `WindowsPath.normalize`).
    let bytes: Vec<u16> = path.encode_utf16().collect();
    let root_len = root.encode_utf16().count().min(bytes.len());
    let mut offsets = Vec::new();
    let mut start = root_len;
    let mut i = root_len;
    while i < bytes.len() {
        if bytes[i] == u16::from(b'\\') {
            if start != i {
                offsets.push(start as i32);
            }
            start = i + 1;
        }
        i += 1;
    }
    if start != i {
        offsets.push(start as i32);
    }
    offsets
}

/// Allocate a synthetic Path at the platform layout's width and store `text`
/// into it, with no normalisation. [`p57_alloc_path`] is the normalising entry
/// point; this one is for producers that have already built an exact string
/// (the jar/jrt encodings, whose sentinel form must not be rewritten).
/// Allocate an object of the platform `Path` class, WIDE ENOUGH for the
/// resolved path layout.
///
/// Both path allocators called `alloc_concrete(.., [impl_class()],
/// "java/nio/file/Path", 0)` on the stated assumption that "every slot this
/// object needs is already a declared field on the concrete class". That holds
/// only when the concrete class EXISTS. In a synthetic-JDK build there is no
/// `sun.nio.fs.WindowsPath` / `UnixPath` to load, so `alloc_concrete` falls
/// back to the INTERFACE `java.nio.file.Path` -- which declares no fields --
/// and the object got `0 + 0` = ZERO slots. Every write of the path string and
/// the owning FileSystem was then dropped as out of bounds, and every read came
/// back empty: `Path.of("/tmp/test.txt").toString()` was "" (the empty string
/// is `p57_read_path`'s re-entrancy guard, reached once the slot read failed).
///
/// The layout already states its minimum: `PathSlots::width`. `alloc_concrete`
/// returns `base`, the resolved class's own field count, which separates the
/// two cases exactly -- the real class has `base >= width`; the interface
/// fallback has `base == 0`. So when `base < width`, allocate again with the
/// missing slots appended. The first, zero-slot object is simply garbage. The
/// real-class path is unchanged: one allocation, no appended slots.
pub fn alloc_platform_path_object(ctx: &mut dyn NativeContext) -> ObjectRef {
    let slots = p57_path_slots_init(ctx);
    let impls = [cratonvm_native_api::path_layout::impl_class()];
    let first =
        cratonvm_native_io::concrete_receiver::alloc_concrete(ctx, &impls, "java/nio/file/Path", 0);
    if first.base >= slots.width {
        return first.obj;
    }
    cratonvm_native_io::concrete_receiver::alloc_concrete(
        ctx,
        &impls,
        "java/nio/file/Path",
        slots.width - first.base,
    )
    .obj
}

pub fn p57_alloc_path_raw(
    ctx: &mut dyn NativeContext,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // Same carrier-identity fix as `p57_alloc_path` (see its own doc) — this is
    // the OTHER producer of `java/nio/file/Path` receivers (jar/jrt entries),
    // and a second interface-stamped mint site would reopen the same
    // `instanceof UnixPath`/`WindowsPath` gap for every path this one builds.
    let obj = alloc_platform_path_object(ctx);
    p57_write_path_fields(ctx, obj, text)
}

/// Field index on a synthetic `java/nio/file/FileSystem` that, when set, holds
/// the OS path of a mounted JAR (see `newFileSystem`). Field 0 is the separator.
pub const P57_FS_JAR_FIELD: usize = 1;

/// Field index on a synthetic `java/nio/file/FileSystem` that, when set, holds
/// the `java.home` of a mounted runtime-image (`jrt:`) filesystem. Field 0 is
/// the separator, field 1 the mounted-JAR path; these are mutually exclusive.
pub const P57_FS_JRT_FIELD: usize = 2;

/// Field index on a synthetic `java/nio/file/FileSystem` holding the single
/// `FileSystemProvider` object that `FileSystem.provider()` hands out for THIS
/// filesystem. Lazily populated by `p57_fs_provider`.
///
/// Why the provider lives on the FileSystem rather than in a Rust-side cache:
/// a `OnceLock<ObjectRef>` would be both process-global (surviving across VMs
/// in-process) and invisible to the GC as a root. The owning FileSystem is
/// already a GC root by virtue of the `FileSystems$DefaultFileSystemHolder.
/// defaultFileSystem` static stash that `p57_default_filesystem_singleton`
/// writes, so hanging the provider off it inherits correct rooting and correct
/// VM scoping for free.
///
/// HotSpot's contract, measured (`scratchpad/c14/FsIdentity.java` on Temurin
/// 25.0.3.9): `getDefault().provider() == getDefault().provider()`,
/// `== DefaultFileSystemProvider.instance()`, and
/// `== FileSystemProvider.installedProviders().get(0)` are all `true`. Before
/// this field existed every one of those minted a fresh object and every one
/// of those identities was false.
/// docs/feature-designs/native-dispatch-architecture.md
pub const P57_FS_PROVIDER_FIELD: usize = 3;

/// Instance-slot count of a synthetic `java/nio/file/FileSystem`:
/// 0 = separator, 1 = mounted-JAR path, 2 = mounted jrt `java.home`,
/// 3 = the provider singleton. Named so the three allocation sites cannot
/// drift from each other or from `P57_FS_PROVIDER_FIELD` (a synthetic class's
/// width being declared in several places is a recorded defect family).
pub const P57_FS_SLOTS: usize = 4;

/// True if `fs` is shaped like this VM's OWN synthetic (jar/jrt-capable)
/// FileSystem object -- i.e. has at least [`P57_FS_SLOTS`] fields.
///
/// **Why this guard exists, and why every `P57_FS_JAR_FIELD`/
/// `P57_FS_JRT_FIELD` reader must go through it.** Since the 2026-09-17
/// default-FileSystem carrier fix, `fs` can ALSO be a genuinely concrete
/// `sun.nio.fs.WindowsFileSystem` (3 real fields: `provider`,
/// `defaultDirectory`, `defaultRoot` -- see `native-api::filesystem_layout`).
/// That receiver is narrower than `P57_FS_SLOTS`, so reading index 1 or 2 off
/// it would silently read one of ITS real fields instead of "is this
/// virtual" -- exactly the type-confused-slot species `path_layout.rs`
/// closed for `Path` one class over. The real default FileSystem is never
/// virtual, so treating "narrower than the synthetic shape" as "definitely
/// not jar/jrt" is correct, not merely safe.
pub fn p57_fs_is_synthetic_shaped(ctx: &dyn NativeContext, fs: ObjectRef) -> bool {
    if ctx.object_num_fields(fs) < P57_FS_SLOTS {
        return false;
    }
    // Width alone stopped being enough once the Linux default FileSystem became
    // a real `sun.nio.fs.LinuxFileSystem` (`p57_alloc_default_filesystem_unix`):
    // it declares FOUR instance fields -- `provider`, `defaultDirectory`,
    // `needToResolveAgainstDefaultDirectory`, `rootDirectory` -- exactly
    // `P57_FS_SLOTS`, so a width test read `defaultDirectory` as "the mounted
    // JAR" and reported the platform's own file system as virtual. The
    // synthetic shape is the abstract `java/nio/file/FileSystem` by name.
    ctx.class_name_arc_of_id(ctx.class_id_of_object(fs))
        .as_deref()
        .is_some_and(|n| n == "java/nio/file/FileSystem")
}

/// True if `fs` is a mounted-jar or runtime-image (virtual) FileSystem. Safe
/// on any receiver shape, including a genuinely concrete default FileSystem
/// (which is never virtual and is never read past its own real fields) --
/// see [`p57_fs_is_synthetic_shaped`].
pub fn p57_fs_is_virtual(ctx: &dyn NativeContext, fs: ObjectRef) -> bool {
    p57_fs_is_synthetic_shaped(ctx, fs)
        && (matches!(ctx.get_field(fs, P57_FS_JAR_FIELD), Value::Object(Some(_)))
            || matches!(ctx.get_field(fs, P57_FS_JRT_FIELD), Value::Object(Some(_))))
}

/// Whether `obj` is a Path of the class this crate mints its `Path`s as --
/// the platform `Path` implementation, or `java/nio/file/Path` itself when a
/// synthetic-JDK build has no platform class to stamp -- so that its text is
/// in the slots `p57_read_path` reads.
pub fn p57_is_path_carrier(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(obj);
    ctx.class_name_of_id(cid).is_some_and(|name| {
        name == cratonvm_native_api::path_layout::impl_class() || name == "java/nio/file/Path"
    })
}

/// Native equivalent of `java.nio.file.Path.toString()`, tuned for synthetic
/// and real `java/nio/file/Path` values used by Javac/ZipFS and JRT paths.
/// This avoids going back through virtual `Path.toString` dispatch, keeping
/// file-bridge paths and archive entry identity stable for real-JDK callers.
///
/// Only for a Path of THIS crate's carrier class (the platform `UnixPath` /
/// `WindowsPath`, or the bare interface a synthetic-JDK build falls back to),
/// whose text lives in the slots `p57_read_path` reads. Any other `Path` --
/// `jdk.nio.zipfs.ZipPath`, `jdk.internal.jrtfs.JrtPath`, an application's
/// own implementation -- has a `toString()` of its own and no such slot, and
/// the slot read answered `""` for every one of them: once `jar:`/`jrt:` were
/// served by the real providers (the archive-provider wave in
/// `native-api/src/retired_shadow.rs`), `"x" + zipPath` string concatenation
/// and javac's `PathFileObject$JarFileObject` binary names both went empty,
/// and an in-process `javac` could find no class in a classpath jar. Under
/// `--real-jdk` such a receiver is answered by its own `toString()` (which
/// `invoke_on_class_shared_inner` no longer redirects back here for a
/// non-carrier Path); `--compatible` is unchanged.
pub fn p57_path_display_string(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    if ctx.is_jdk_only() && !p57_is_path_carrier(ctx, this) {
        let pin = ctx.pin_native_root(this);
        let text = ctx.invoke_virtual(this, "toString", "()Ljava/lang/String;", &[]);
        ctx.unpin_native_roots(pin);
        return match text {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
    }
    p57_path_slot_display_string(ctx, this)
}

/// The carrier half of [`p57_path_display_string`]: the display text read
/// from the Path's own slots. Also the body of the registered
/// `java/nio/file/Path.toString` bridge, which used to be a verbatim copy of
/// it. That bridge calls this half, not the whole function: the strict-mode
/// arm above re-enters virtual `toString`, which is not a hop to make from
/// inside a `toString` registration.
pub fn p57_path_slot_display_string(ctx: &mut dyn NativeContext, mut this: ObjectRef) -> String {
    // `this` is read again below (`path_owned_by_virtual_fs`, the stored
    // slot), so the read hands it back current (gc-common w21-b).
    let p = p57_read_path(ctx, &mut this);
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
        // Host-FS path on Windows: the slot already holds the JDK's own `\`
        // form (`path_layout::stored_form`), which is exactly what a real
        // `WindowsPath.toString()` returns — including the closing `\` of a
        // UNC root (`\\srv\share\`) that `file_normalise_path` would trim.
        None if cfg!(windows) && p57_path_slot_string_stored(ctx, this).is_some() => {
            let raw = p57_path_slot_string_stored(ctx, this).unwrap_or_default();
            cratonvm_native_api::path_layout::stored_form(&raw).into_owned()
        }
        // Host-FS path elsewhere: render the OS-native separator.
        None => file_normalise_path(&p),
    }
}

/// Register one of the five `sun/nio/fs/WindowsPath` Intrinsics that serve a
/// jar/jrt Path wearing the platform `Path` class -- `resolve`, `getFileName`,
/// `equals`, `hashCode`, `toRealPath` -- in `--compatible` mode only.
///
/// Each triple is RETIRED (its real bytecode is what a host `WindowsPath`
/// runs), and each Intrinsic existed because this VM's synthetic `jar:`/`jrt:`
/// filesystems minted their Paths as `WindowsPath` carriers, which the real
/// bytecode cannot read (the collisions in
/// `docs/feature-designs/native-dispatch-architecture.md`).
/// Under `--real-jdk` those filesystems are gone: `jar:` and `jrt:` are the
/// real `jdk.nio.zipfs` / `jdk.internal.jrtfs` providers (the archive-provider
/// wave in `native-api/src/retired_shadow.rs`), no `WindowsPath` carries an
/// encoded archive path, and every call took the Intrinsic's
/// `invoke_virtual_bytecode_only` arm -- the Intrinsic only stood in front of
/// the bytecode the retirement exists to reach, and
/// `no_retired_triple_survives_the_strict_boot` listed all five as INERT. So
/// the strict registry does not carry them. `--compatible` still mints the
/// synthetic filesystems and keeps every one, unchanged.
pub fn register_vfs_windows_path_intrinsic(
    r: &mut NativeMethodRegistry,
    method: &str,
    descriptor: &str,
    callback: cratonvm_native_api::NativeCallback,
) {
    if r.compatibility_mode().is_jdk_only() {
        return;
    }
    r.register(
        "sun/nio/fs/WindowsPath",
        method,
        descriptor,
        callback,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
}

/// SBR wave 2, lane `fs`: under `--real-jdk`, hand a call that reached one of
/// the ABSTRACT `java/nio/file/spi/FileSystemProvider` registrations back to
/// the receiver's own bytecode when the receiver's triple is retired.
///
/// `RETIRED_SHADOW_SBR_FS_TRIPLES` retires the `LinuxFileSystemProvider`
/// copies of the provider rows, but the same callbacks stay registered on the
/// abstract class, where a native binds an abstract method and is not a
/// shadow. MEASURED on the first trial binary: an `invokevirtual
/// FileSystemProvider.newDirectoryStream` on the real default provider still
/// ran the native (which mints a `UnixSecureDirectoryStream` without its
/// constructor -- `NullPointerException: this.ds` from the real `close()`),
/// and `newFileSystem(URI)` still mounted a filesystem where HotSpot throws
/// `FileSystemAlreadyExistsException`. The abstract rows are not this lane's
/// to retire (a jar/jrt provider stamped with the abstract class has no body
/// to fall back to), so the callback asks the one question that decides it:
/// is THIS receiver's triple retired? Then its real bytecode is
/// authoritative, and runs. `CRATONVM_UNRETIRE_NATIVE_SHADOW` flips the
/// answer with the table, so the unretire arm reproduces the control.
/// `--compatible` never takes the branch.
pub fn yield_to_retired_receiver(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    if !ctx.is_jdk_only() {
        return None;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let class = ctx.class_name_arc_of_id(ctx.class_id_of_object(this))?;
    if !cratonvm_native_api::retired_shadow::triple_is_retired_shadow(&class, method, descriptor) {
        return None;
    }
    // Re-entrancy guard. A `super.m(..)` from the receiver's own bytecode is
    // an `invokespecial` that must NOT be re-dispatched virtually: if one ever
    // lands back in this callback (a superclass row still bound to it), the
    // virtual re-dispatch would recurse forever -- measured on the third
    // trial binary before `UnixFileSystemProvider`'s rows joined the table.
    // A nested entry for the same method runs the native instead: finite, and
    // the answer the control gave.
    thread_local! {
        static YIELDING: std::cell::RefCell<Vec<(String, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let nested = YIELDING.with(|y| {
        y.borrow()
            .iter()
            .any(|(m, d)| m == method && d == descriptor)
    });
    if nested {
        return None;
    }
    YIELDING.with(|y| {
        y.borrow_mut()
            .push((method.to_string(), descriptor.to_string()))
    });
    let result = ctx.invoke_virtual_bytecode_only(this, method, descriptor, &args[1..]);
    YIELDING.with(|y| {
        y.borrow_mut().pop();
    });
    Some(result)
}

/// The directory intrinsics own only objects carrying their appended map.
/// A stream constructed by JDK bytecode has the real field count and must
/// keep its own iterator, close, and secure-directory operations.
pub fn real_directory_stream_bytecode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(this))) => *this,
        _ => return None,
    };
    let base =
        cratonvm_native_api::appended_slots::base_for_class_id(ctx, ctx.class_id_of_object(this));
    if ctx.object_num_fields(this) >= base + DIR_STREAM_SLOTS {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(this, method, descriptor, &args[1..]))
}

/// `--compatible`: hand a HOST directory listing on a platform provider to the
/// receiver's own JDK body (`UnixFileSystemProvider.newDirectoryStream`), so the
/// stream is a real `UnixSecureDirectoryStream` built by its constructor.
///
/// Since the FFM merge (`resolve_step1_native`: concrete bytecode is
/// authoritative over a `Bridge` or `SyntheticStub` in both policies) the
/// minted stream's `iterator()`/`close()` run the real class's Code, which reads
/// the `ds`/`dfd` fields only the constructor sets: `JceSecurity.<clinit>`
/// (every `Cipher`/`Mac`/SSL use) and the JFR repository scan threw
/// `NullPointerException: this.ds` on Linux. A mint stays for the jar/jrt
/// virtual directories, which mint the INTERFACE and keep this file's natives,
/// and for a receiver that is not a platform provider. `--real-jdk` never
/// reaches this (its provider rows are retired, [`yield_to_retired_receiver`]).
pub fn platform_provider_lists_host_dir(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    path_obj: ObjectRef,
    path: &str,
) -> Option<MethodCallResult> {
    if ctx.is_jdk_only() || jarfs_decode(path).is_some() || jrtfs_decode(path).is_some() {
        return None;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let provider = ctx.class_name_arc_of_id(ctx.class_id_of_object(this))?;
    let path_class = ctx.class_name_arc_of_id(ctx.class_id_of_object(path_obj))?;
    if !provider.starts_with("sun/nio/fs/")
        || ctx.is_class_synthetic_stub(&provider)
        || &*path_class != cratonvm_native_api::path_layout::impl_class()
    {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(
        this,
        "newDirectoryStream",
        "(Ljava/nio/file/Path;Ljava/nio/file/DirectoryStream$Filter;)Ljava/nio/file/DirectoryStream;",
        &[Value::Object(Some(path_obj)), args.get(2).copied().unwrap_or(Value::Object(None))],
    ))
}

/// JVMS 6.5 (nio21 close), `JdkArchiveFs`: under `--compatible` with the row,
/// a native that takes a `Path` hands the call to the method's own JDK body
/// when a `Path` argument is not this VM's carrier -- a `jdk.nio.zipfs.ZipPath`
/// or `jdk.internal.jrtfs.JrtPath` of a mount the JDK's own provider built, an
/// application's own `Path` -- because the native reads the path TEXT from the
/// carrier's slots and would answer for a file that is not the archive entry.
/// `None` when the row is off or every `Path` argument is a carrier.
pub fn yield_to_foreign_path(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    owner: &str,
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    if !ctx.aligned(cratonvm_types::HotSpotAlignment::JdkArchiveFs) {
        return None;
    }
    let mut foreign = false;
    for a in args {
        if let Value::Object(Some(o)) = a {
            if p57_is_path_object(ctx, *o) && !p57_is_path_carrier(ctx, *o) {
                foreign = true;
                break;
            }
        }
    }
    if !foreign {
        return None;
    }
    // gci i6/jd: a `java/nio/file/Path` row is an instance method of the
    // INTERFACE, and most of them are abstract there (`toString`,
    // `getFileName`, ...): `invokespecial Path.m` has no body to run and fell
    // to `Object.toString()`'s `jdk.internal.jrtfs.JrtPath@<hash>`. A call that
    // reached the row is an interface call on its receiver, so when the
    // receiver itself is the foreign Path, HotSpot runs the receiver's own
    // implementation: dispatch it virtually, bytecode only (no native can
    // answer again). `Files` rows (static) and a carrier receiver with a
    // foreign argument keep the special call, and so does a default method the
    // receiver's class chain does not override (the interface body is then
    // the receiver's implementation).
    if owner == "java/nio/file/Path" {
        if let Some(Value::Object(Some(recv))) = args.first().copied() {
            if p57_is_path_object(ctx, recv)
                && !p57_is_path_carrier(ctx, recv)
                && receiver_chain_declares(ctx, recv, method, descriptor)
            {
                if false {
                    let class = ctx
                        .class_name_arc_of_id(ctx.class_id_of_object(recv))
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    eprintln!(
                        "[FOREIGN-PATH] Path.{method}{descriptor} yields to {class}'s own body"
                    );
                }
                return Some(ctx.invoke_virtual_bytecode_only(
                    recv,
                    method,
                    descriptor,
                    &args[1..],
                ));
            }
        }
    }
    Some(ctx.invoke_special_bytecode_only(owner, method, descriptor, args))
}

/// Does `recv`'s class, or a superclass below `java/lang/Object`, itself
/// declare `method` `descriptor`? (The walk `p57_foreign_path_to_string`
/// makes, for any method.)
pub fn receiver_chain_declares(
    ctx: &mut dyn NativeContext,
    recv: ObjectRef,
    method: &str,
    descriptor: &str,
) -> bool {
    let mut cid = Some(ctx.class_id_of_object(recv));
    while let Some(c) = cid {
        if ctx.class_name_arc_of_id(c).as_deref() == Some("java/lang/Object") {
            return false;
        }
        if ctx.class_declares_method(c, method, descriptor) {
            return true;
        }
        cid = ctx.superclass_of(c);
    }
    false
}

